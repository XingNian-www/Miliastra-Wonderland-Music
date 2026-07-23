mod batch;
mod engine;

use std::collections::VecDeque;
use std::sync::mpsc::{self, SyncSender};
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
    image: DynamicImage,
    response: SyncSender<Result<Vec<OcrLine>>>,
}

struct OcrQueueState {
    accepting: bool,
    next_sequence: u64,
    jobs: VecDeque<OcrJob>,
}

struct OcrChannel {
    state: Mutex<OcrQueueState>,
    available: Condvar,
    capacity: usize,
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
    ) -> Result<Vec<OcrLine>> {
        let (response, receiver) = mpsc::sync_channel(1);
        {
            let mut state = self
                .channel
                .state
                .lock()
                .map_err(|_| anyhow!("OCR runtime queue mutex poisoned"))?;
            if !state.accepting {
                return Err(anyhow!("OCR runtime is stopped"));
            }
            if state.jobs.len() >= self.channel.capacity {
                return Err(anyhow!("OCR runtime queue is full"));
            }
            state.next_sequence = state.next_sequence.wrapping_add(1);
            let sequence = state.next_sequence;
            state.jobs.push_back(OcrJob {
                priority,
                sequence,
                image,
                response,
            });
            self.channel.available.notify_one();
        }
        receiver
            .recv()
            .map_err(|_| anyhow!("OCR runtime stopped before returning a result"))?
    }

    pub(crate) fn merged_text(
        &self,
        image: DynamicImage,
        same_line_y_tolerance: i32,
        priority: OcrPriority,
    ) -> Result<String> {
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
}

impl OcrRuntime {
    pub(crate) fn start(device: impl OcrDevice, capacity: usize) -> Result<Self> {
        if capacity == 0 {
            return Err(anyhow!(
                "OCR runtime queue capacity must be greater than zero"
            ));
        }
        let channel = Arc::new(OcrChannel {
            state: Mutex::new(OcrQueueState {
                accepting: true,
                next_sequence: 0,
                jobs: VecDeque::new(),
            }),
            available: Condvar::new(),
            capacity,
        });
        let worker_channel = channel.clone();
        let worker = thread::Builder::new()
            .name("ocr-runtime".to_string())
            .spawn(move || run_ocr_runtime(device, worker_channel))?;
        Ok(Self {
            handle: OcrRuntimeHandle { channel },
            worker: Some(worker),
        })
    }

    pub(crate) fn handle(&self) -> OcrRuntimeHandle {
        self.handle.clone()
    }

    pub(crate) fn shutdown(mut self) -> Result<()> {
        self.stop_worker()
    }

    fn stop_worker(&mut self) -> Result<()> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        if let Ok(mut state) = self.handle.channel.state.lock() {
            state.accepting = false;
            self.handle.channel.available.notify_all();
        }
        worker
            .join()
            .map_err(|_| anyhow!("OCR runtime worker panicked"))
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
            state.jobs.remove(index)
        };
        let Some(job) = job else {
            continue;
        };
        let result = device.recognize_lines(&job.image);
        let _ = job.response.send(result);
    }
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn caller_recognizes_owned_image_through_ocr_runtime() {
        let runtime = OcrRuntime::start(FixedOcrDevice, 2).unwrap();

        let lines = runtime
            .handle()
            .recognize_lines(DynamicImage::new_rgba8(7, 5), OcrPriority::ChatObservation)
            .unwrap();

        assert_eq!(lines[0].text, "7x5");
        runtime.shutdown().unwrap();
    }
}
