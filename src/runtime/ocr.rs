mod batch;
mod engine;

use std::collections::VecDeque;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use image::DynamicImage;

pub(crate) use batch::{OcrImageBlock, batch_recognize_blocks};
pub(crate) use engine::{
    OcrArgs, OcrBackendProbeStatus, OcrEngineBackend, OcrLine, ResolvedOcrArgs, make_ocr_engine,
    merge_ocr_lines, probe_ocr_backend_support, recognize_lines,
};

const OCR_REBUILD_INTERVAL: Duration = Duration::from_secs(60 * 60);
const OCR_REBUILD_RETRY_INTERVAL: Duration = Duration::from_secs(5 * 60);
#[cfg(test)]
const DEFAULT_OCR_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(test)]
const DEFAULT_OCR_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

type OcrRuntimeResult<T> = std::result::Result<T, OcrRuntimeError>;

#[derive(Debug)]
pub(crate) enum OcrRuntimeError {
    QueueFull,
    Stopped,
    TimedOut,
    Inference(anyhow::Error),
    Unavailable(&'static str),
}

impl Display for OcrRuntimeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QueueFull => formatter.write_str("OCR runtime queue is full"),
            Self::Stopped => formatter.write_str("OCR runtime is stopped"),
            Self::TimedOut => formatter.write_str("OCR runtime request timed out"),
            Self::Inference(error) => write!(formatter, "OCR inference failed: {error:#}"),
            Self::Unavailable(reason) => write!(formatter, "OCR runtime is unavailable: {reason}"),
        }
    }
}

impl Error for OcrRuntimeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Inference(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct OcrRuntimeConfig {
    pub(crate) queue_capacity: usize,
    pub(crate) request_timeout: Duration,
    pub(crate) shutdown_timeout: Duration,
}

pub(crate) trait OcrDevice: Send + 'static {
    fn recognize_lines(&mut self, image: &DynamicImage) -> Result<Vec<OcrLine>>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum OcrPriority {
    UiConfirmation,
    ChatObservation,
    Diagnostic,
}

struct OcrJob {
    priority: OcrPriority,
    sequence: u64,
    deadline: Instant,
    image: DynamicImage,
    response: SyncSender<OcrRuntimeResult<Vec<OcrLine>>>,
}

struct OcrQueueState {
    accepting: bool,
    next_sequence: u64,
    jobs: VecDeque<OcrJob>,
    active_response: Option<SyncSender<OcrRuntimeResult<Vec<OcrLine>>>>,
}

struct OcrChannel {
    state: Mutex<OcrQueueState>,
    available: Condvar,
    capacity: usize,
    request_timeout: Duration,
}

#[derive(Clone)]
pub(crate) struct OcrRuntimeHandle {
    channel: Arc<OcrChannel>,
}

impl OcrRuntimeHandle {
    pub(crate) fn recognize_lines(
        &self,
        image: DynamicImage,
        priority: OcrPriority,
    ) -> OcrRuntimeResult<Vec<OcrLine>> {
        let (response, receiver) = mpsc::sync_channel(1);
        let deadline = Instant::now()
            .checked_add(self.channel.request_timeout)
            .ok_or(OcrRuntimeError::Unavailable("request deadline overflowed"))?;
        let sequence = {
            let mut state = self
                .channel
                .state
                .lock()
                .map_err(|_| OcrRuntimeError::Unavailable("queue mutex poisoned"))?;
            if !state.accepting {
                return Err(OcrRuntimeError::Stopped);
            }
            if state.jobs.len() >= self.channel.capacity {
                return Err(OcrRuntimeError::QueueFull);
            }
            state.next_sequence = state.next_sequence.wrapping_add(1);
            let sequence = state.next_sequence;
            state.jobs.push_back(OcrJob {
                priority,
                sequence,
                deadline,
                image,
                response,
            });
            self.channel.available.notify_one();
            sequence
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Ok(mut state) = self.channel.state.lock()
                    && let Some(index) = state.jobs.iter().position(|job| job.sequence == sequence)
                {
                    state.jobs.remove(index);
                }
                Err(OcrRuntimeError::TimedOut)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(OcrRuntimeError::Stopped),
        }
    }

    pub(crate) fn merged_text(
        &self,
        image: DynamicImage,
        same_line_y_tolerance: i32,
        priority: OcrPriority,
    ) -> OcrRuntimeResult<String> {
        Ok(merge_ocr_lines(
            self.recognize_lines(image, priority)?,
            same_line_y_tolerance,
        ))
    }
}

pub(crate) struct ProductionOcrDevice {
    args: ResolvedOcrArgs,
    engine: OcrEngineBackend,
    rebuild_due_at: Instant,
}

impl ProductionOcrDevice {
    pub(crate) fn new(args: ResolvedOcrArgs) -> Result<Self> {
        let engine = make_ocr_engine(&args)?;
        Ok(Self {
            args,
            engine,
            rebuild_due_at: Instant::now() + OCR_REBUILD_INTERVAL,
        })
    }

    fn rebuild_if_due(&mut self) {
        if Instant::now() < self.rebuild_due_at {
            return;
        }
        log::info!("OCR 引擎运行超过 1 小时，开始重建");
        let started = Instant::now();
        match make_ocr_engine(&self.args) {
            Ok(engine) => {
                self.engine = engine;
                self.rebuild_due_at = Instant::now() + OCR_REBUILD_INTERVAL;
                log::info!("OCR 引擎重建完成");
                log::info!(target: "timing", "OCR 引擎重建耗时: {}ms", started.elapsed().as_millis());
            }
            Err(error) => {
                self.rebuild_due_at = Instant::now() + OCR_REBUILD_RETRY_INTERVAL;
                log::error!("OCR 引擎重建失败，继续使用旧引擎，5分钟后重试: {error:#}");
            }
        }
    }
}

impl OcrDevice for ProductionOcrDevice {
    fn recognize_lines(&mut self, image: &DynamicImage) -> Result<Vec<OcrLine>> {
        self.rebuild_if_due();
        recognize_lines(&mut self.engine, image)
    }
}

pub(crate) struct OcrRuntime {
    handle: OcrRuntimeHandle,
    worker: Option<JoinHandle<()>>,
    worker_finished: Receiver<()>,
    shutdown_timeout: Duration,
    /// 关闭超时后放弃的 worker,后续启动/关闭时回收已结束的线程。
    abandoned_workers: Vec<JoinHandle<()>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct OcrShutdownReport {
    pub(crate) timed_out: bool,
}

#[derive(Clone, Copy)]
enum OcrStopCause {
    Shutdown,
    WorkerPanicked,
}

impl OcrStopCause {
    fn error(self) -> OcrRuntimeError {
        match self {
            Self::Shutdown => OcrRuntimeError::Stopped,
            Self::WorkerPanicked => OcrRuntimeError::Unavailable("worker panicked"),
        }
    }
}

impl OcrRuntime {
    #[cfg(test)]
    pub(crate) fn start(device: impl OcrDevice, capacity: usize) -> Result<Self> {
        Self::start_configured(
            device,
            OcrRuntimeConfig {
                queue_capacity: capacity,
                request_timeout: DEFAULT_OCR_REQUEST_TIMEOUT,
                shutdown_timeout: DEFAULT_OCR_SHUTDOWN_TIMEOUT,
            },
        )
    }

    pub(crate) fn start_configured(
        device: impl OcrDevice,
        config: OcrRuntimeConfig,
    ) -> Result<Self> {
        if config.queue_capacity == 0 {
            return Err(anyhow!(
                "OCR runtime queue capacity must be greater than zero"
            ));
        }
        if config.request_timeout.is_zero() {
            return Err(anyhow!(
                "OCR runtime request timeout must be greater than zero"
            ));
        }
        if config.shutdown_timeout.is_zero() {
            return Err(anyhow!(
                "OCR runtime shutdown timeout must be greater than zero"
            ));
        }
        let channel = Arc::new(OcrChannel {
            state: Mutex::new(OcrQueueState {
                accepting: true,
                next_sequence: 0,
                jobs: VecDeque::new(),
                active_response: None,
            }),
            available: Condvar::new(),
            capacity: config.queue_capacity,
            request_timeout: config.request_timeout,
        });
        let worker_channel = channel.clone();
        let (worker_finished_tx, worker_finished) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("ocr-runtime".to_string())
            .spawn(move || {
                let runtime_channel = worker_channel.clone();
                let result = catch_unwind(AssertUnwindSafe(|| {
                    run_ocr_runtime(device, runtime_channel);
                }));
                if result.is_err() {
                    stop_ocr_channel(&worker_channel, OcrStopCause::WorkerPanicked);
                }
                let _ = worker_finished_tx.send(());
                if let Err(payload) = result {
                    resume_unwind(payload);
                }
            })?;
        Ok(Self {
            handle: OcrRuntimeHandle { channel },
            worker: Some(worker),
            worker_finished,
            shutdown_timeout: config.shutdown_timeout,
            abandoned_workers: Vec::new(),
        })
    }

    pub(crate) fn handle(&self) -> OcrRuntimeHandle {
        self.handle.clone()
    }

    pub(crate) fn shutdown(mut self) -> Result<OcrShutdownReport> {
        self.stop_worker()
    }

    fn stop_worker(&mut self) -> Result<OcrShutdownReport> {
        // 先回收上一轮超时放弃的 worker(已结束的直接 join,避免线程泄漏累积)。
        self.reap_abandoned_workers();
        let Some(worker) = self.worker.take() else {
            return Ok(OcrShutdownReport::default());
        };
        stop_ocr_channel(&self.handle.channel, OcrStopCause::Shutdown);
        match self.worker_finished.recv_timeout(self.shutdown_timeout) {
            Ok(()) => {
                worker
                    .join()
                    .map_err(|_| anyhow!("OCR runtime worker panicked"))?;
                Ok(OcrShutdownReport { timed_out: false })
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // 保留句柄供后续回收,不再直接丢弃。
                self.abandoned_workers.push(worker);
                if self.abandoned_workers.len() > 8 {
                    log::error!(
                        "OCR worker 累计 {} 个未退出线程,可能存在原生推理卡死",
                        self.abandoned_workers.len()
                    );
                }
                Ok(OcrShutdownReport { timed_out: true })
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                worker
                    .join()
                    .map_err(|_| anyhow!("OCR runtime worker panicked"))?;
                Err(anyhow!(
                    "OCR runtime worker stopped without a completion signal"
                ))
            }
        }
    }

    fn reap_abandoned_workers(&mut self) {
        let mut index = 0;
        while index < self.abandoned_workers.len() {
            if self.abandoned_workers[index].is_finished() {
                let worker = self.abandoned_workers.swap_remove(index);
                let _ = worker.join();
            } else {
                index += 1;
            }
        }
    }
}

fn stop_ocr_channel(channel: &OcrChannel, cause: OcrStopCause) {
    let (queued_responses, active_response) = {
        let mut state = match channel.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.accepting = false;
        let queued_responses = state
            .jobs
            .drain(..)
            .map(|job| job.response)
            .collect::<Vec<_>>();
        let active_response = state.active_response.take();
        channel.available.notify_all();
        (queued_responses, active_response)
    };
    for response in queued_responses {
        let _ = response.try_send(Err(cause.error()));
    }
    if let Some(response) = active_response {
        let _ = response.try_send(Err(cause.error()));
    }
}

impl Drop for OcrRuntime {
    fn drop(&mut self) {
        let _ = self.stop_worker();
    }
}

fn run_ocr_runtime(mut device: impl OcrDevice, channel: Arc<OcrChannel>) {
    loop {
        let job = {
            let mut state = match channel.state.lock() {
                Ok(state) => state,
                Err(_) => return,
            };
            while state.jobs.is_empty() && state.accepting {
                state = match channel.available.wait(state) {
                    Ok(state) => state,
                    Err(_) => return,
                };
            }
            if state.jobs.is_empty() {
                return;
            }
            let index = state
                .jobs
                .iter()
                .enumerate()
                .min_by_key(|(_, job)| (job.priority, job.sequence))
                .map(|(index, _)| index)
                .unwrap_or(0);
            let job = state.jobs.remove(index);
            if let Some(job) = &job
                && Instant::now() < job.deadline
            {
                state.active_response = Some(job.response.clone());
            }
            job
        };
        let Some(job) = job else {
            continue;
        };
        if Instant::now() >= job.deadline {
            let _ = job.response.try_send(Err(OcrRuntimeError::TimedOut));
            continue;
        }
        let result = device
            .recognize_lines(&job.image)
            .map_err(OcrRuntimeError::Inference);
        let should_respond = match channel.state.lock() {
            Ok(mut state) => {
                state.active_response.take();
                state.accepting
            }
            Err(_) => false,
        };
        if should_respond {
            let _ = job.response.try_send(result);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use anyhow::Result;
    use image::DynamicImage;

    use super::*;
    use crate::runtime::ocr::OcrLine;
    use crate::ui::geometry::Rect;

    struct FixedOcrDevice;

    impl OcrDevice for FixedOcrDevice {
        fn recognize_lines(&mut self, image: &DynamicImage) -> Result<Vec<OcrLine>> {
            Ok(vec![OcrLine {
                text: format!("{}x{}", image.width(), image.height()),
                confidence: 1.0,
                bbox: Rect::new(0, 0, image.width(), image.height()),
            }])
        }
    }

    struct BlockingOcrDevice {
        started: mpsc::SyncSender<()>,
        release: mpsc::Receiver<()>,
    }

    impl OcrDevice for BlockingOcrDevice {
        fn recognize_lines(&mut self, _image: &DynamicImage) -> Result<Vec<OcrLine>> {
            self.started.send(()).unwrap();
            self.release.recv().unwrap();
            Ok(Vec::new())
        }
    }

    struct FirstRequestBlockingOcrDevice {
        observed_width: mpsc::SyncSender<u32>,
        release_first: mpsc::Receiver<()>,
    }

    impl OcrDevice for FirstRequestBlockingOcrDevice {
        fn recognize_lines(&mut self, image: &DynamicImage) -> Result<Vec<OcrLine>> {
            self.observed_width.send(image.width()).unwrap();
            if image.width() == 1 {
                self.release_first.recv().unwrap();
            }
            Ok(vec![OcrLine {
                text: image.width().to_string(),
                confidence: 1.0,
                bbox: Rect::new(0, 0, image.width(), image.height()),
            }])
        }
    }

    struct PanickingOcrDevice;

    impl OcrDevice for PanickingOcrDevice {
        fn recognize_lines(&mut self, _image: &DynamicImage) -> Result<Vec<OcrLine>> {
            panic!("simulated OCR worker panic");
        }
    }

    struct FailingOcrDevice;

    impl OcrDevice for FailingOcrDevice {
        fn recognize_lines(&mut self, _image: &DynamicImage) -> Result<Vec<OcrLine>> {
            Err(anyhow!("simulated inference failure"))
        }
    }

    #[test]
    fn caller_recognizes_owned_image_through_ocr_runtime() {
        let runtime = OcrRuntime::start(FixedOcrDevice, 2).unwrap();

        let lines = runtime
            .handle()
            .recognize_lines(DynamicImage::new_rgba8(7, 5), OcrPriority::ChatObservation)
            .unwrap();

        assert_eq!(lines[0].text, "7x5");
        assert!(!runtime.shutdown().unwrap().timed_out);
    }

    #[test]
    fn inference_failure_remains_distinct_from_runtime_failures() {
        let runtime = OcrRuntime::start(FailingOcrDevice, 1).unwrap();

        let error = runtime
            .handle()
            .recognize_lines(DynamicImage::new_rgba8(1, 1), OcrPriority::ChatObservation)
            .expect_err("the device failure should reach the caller");

        let OcrRuntimeError::Inference(source) = error else {
            panic!("expected an inference error");
        };
        assert!(source.to_string().contains("simulated inference failure"));
        assert!(!runtime.shutdown().unwrap().timed_out);
    }

    #[test]
    fn caller_times_out_when_ocr_inference_does_not_finish() {
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let runtime = OcrRuntime::start_configured(
            BlockingOcrDevice {
                started: started_tx,
                release: release_rx,
            },
            OcrRuntimeConfig {
                queue_capacity: 1,
                request_timeout: Duration::from_millis(50),
                shutdown_timeout: Duration::from_secs(1),
            },
        )
        .unwrap();
        let handle = runtime.handle();
        let request = std::thread::spawn(move || {
            handle.recognize_lines(DynamicImage::new_rgba8(1, 1), OcrPriority::ChatObservation)
        });

        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("OCR inference should start");
        let error = request
            .join()
            .unwrap()
            .expect_err("a blocked OCR request should time out");

        assert!(matches!(error, OcrRuntimeError::TimedOut));
        release_tx.send(()).unwrap();
        runtime.shutdown().unwrap();
    }

    #[test]
    fn queued_request_is_removed_when_its_caller_times_out() {
        let (observed_tx, observed_rx) = mpsc::sync_channel(3);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let runtime = OcrRuntime::start_configured(
            FirstRequestBlockingOcrDevice {
                observed_width: observed_tx,
                release_first: release_rx,
            },
            OcrRuntimeConfig {
                queue_capacity: 1,
                request_timeout: Duration::from_millis(200),
                shutdown_timeout: Duration::from_secs(1),
            },
        )
        .unwrap();
        let first_handle = runtime.handle();
        let first = std::thread::spawn(move || {
            first_handle
                .recognize_lines(DynamicImage::new_rgba8(1, 1), OcrPriority::ChatObservation)
        });
        assert_eq!(observed_rx.recv_timeout(Duration::from_secs(1)).unwrap(), 1);

        let second_handle = runtime.handle();
        let second = std::thread::spawn(move || {
            second_handle
                .recognize_lines(DynamicImage::new_rgba8(2, 1), OcrPriority::ChatObservation)
        });
        let second_error = second
            .join()
            .unwrap()
            .expect_err("the queued request should time out");

        let third_handle = runtime.handle();
        let third = std::thread::spawn(move || {
            third_handle
                .recognize_lines(DynamicImage::new_rgba8(3, 1), OcrPriority::ChatObservation)
        });
        release_tx.send(()).unwrap();
        let _ = first.join().unwrap();
        let third_lines = third
            .join()
            .unwrap()
            .expect("the released queue slot should accept the third request");

        assert!(matches!(second_error, OcrRuntimeError::TimedOut));
        assert_eq!(third_lines[0].text, "3");
        assert_eq!(
            observed_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            3,
            "the expired second request must not reach the OCR device"
        );
        runtime.shutdown().unwrap();
    }

    #[test]
    fn shutdown_releases_active_and_queued_callers_and_rejects_new_requests() {
        let (observed_tx, observed_rx) = mpsc::sync_channel(3);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let runtime = OcrRuntime::start_configured(
            FirstRequestBlockingOcrDevice {
                observed_width: observed_tx,
                release_first: release_rx,
            },
            OcrRuntimeConfig {
                queue_capacity: 1,
                request_timeout: Duration::from_secs(5),
                shutdown_timeout: Duration::from_secs(1),
            },
        )
        .unwrap();
        let (active_result_tx, active_result_rx) = mpsc::sync_channel(1);
        let active_handle = runtime.handle();
        let active = std::thread::spawn(move || {
            let result = active_handle
                .recognize_lines(DynamicImage::new_rgba8(1, 1), OcrPriority::ChatObservation);
            active_result_tx.send(result).unwrap();
        });
        assert_eq!(
            observed_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("the active request should reach the OCR device"),
            1
        );

        let (queued_result_tx, queued_result_rx) = mpsc::sync_channel(2);
        let mut queued_threads = Vec::new();
        for width in [2, 3] {
            let handle = runtime.handle();
            let result_tx = queued_result_tx.clone();
            queued_threads.push(std::thread::spawn(move || {
                let result = handle.recognize_lines(
                    DynamicImage::new_rgba8(width, 1),
                    OcrPriority::ChatObservation,
                );
                result_tx.send(result).unwrap();
            }));
        }
        drop(queued_result_tx);
        let rejected_while_full = queued_result_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("one queued candidate should be rejected by the full queue");
        assert!(matches!(
            rejected_while_full,
            Err(OcrRuntimeError::QueueFull)
        ));

        let post_shutdown_handle = runtime.handle();
        let shutdown = std::thread::spawn(move || runtime.shutdown());
        let shutdown_deadline = Instant::now() + Duration::from_secs(1);
        let post_shutdown_error = loop {
            match post_shutdown_handle
                .recognize_lines(DynamicImage::new_rgba8(4, 1), OcrPriority::ChatObservation)
            {
                Err(OcrRuntimeError::QueueFull) if Instant::now() < shutdown_deadline => {
                    std::thread::yield_now();
                }
                result => break result,
            }
        };
        let active_before_release = active_result_rx.recv_timeout(Duration::from_millis(500));
        let queued_before_release = queued_result_rx.recv_timeout(Duration::from_millis(500));

        release_tx.send(()).unwrap();
        active.join().unwrap();
        for thread in queued_threads {
            thread.join().unwrap();
        }
        shutdown.join().unwrap().unwrap();

        assert!(matches!(post_shutdown_error, Err(OcrRuntimeError::Stopped)));
        assert!(matches!(
            active_before_release,
            Ok(Err(OcrRuntimeError::Stopped))
        ));
        assert!(matches!(
            queued_before_release,
            Ok(Err(OcrRuntimeError::Stopped))
        ));
    }

    #[test]
    fn shutdown_reports_timeout_when_native_inference_remains_blocked() {
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let runtime = OcrRuntime::start_configured(
            BlockingOcrDevice {
                started: started_tx,
                release: release_rx,
            },
            OcrRuntimeConfig {
                queue_capacity: 1,
                request_timeout: Duration::from_secs(5),
                shutdown_timeout: Duration::from_millis(50),
            },
        )
        .unwrap();
        let handle = runtime.handle();
        let request = std::thread::spawn(move || {
            handle.recognize_lines(DynamicImage::new_rgba8(1, 1), OcrPriority::ChatObservation)
        });
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("OCR inference should start");

        let report = runtime.shutdown().unwrap();

        assert!(report.timed_out);
        assert!(matches!(
            request.join().unwrap(),
            Err(OcrRuntimeError::Stopped)
        ));
        release_tx.send(()).unwrap();
    }

    #[test]
    fn worker_panic_fails_the_active_request_and_stops_accepting_work() {
        let runtime = OcrRuntime::start_configured(
            PanickingOcrDevice,
            OcrRuntimeConfig {
                queue_capacity: 1,
                request_timeout: Duration::from_millis(200),
                shutdown_timeout: Duration::from_secs(1),
            },
        )
        .unwrap();
        let handle = runtime.handle();

        let active_result =
            handle.recognize_lines(DynamicImage::new_rgba8(1, 1), OcrPriority::ChatObservation);
        let later_result =
            handle.recognize_lines(DynamicImage::new_rgba8(2, 1), OcrPriority::ChatObservation);
        let shutdown_result = runtime.shutdown();

        assert!(matches!(
            active_result,
            Err(OcrRuntimeError::Unavailable("worker panicked"))
        ));
        assert!(matches!(later_result, Err(OcrRuntimeError::Stopped)));
        assert!(
            shutdown_result
                .expect_err("shutdown should preserve the worker panic")
                .to_string()
                .contains("worker panicked")
        );
    }
}
