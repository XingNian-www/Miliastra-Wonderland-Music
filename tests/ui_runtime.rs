use anyhow::Result;
use image::{DynamicImage, GenericImageView};
use miliastra_wonderland_music::runtime::ui::{
    CaptureFrame, FrameDemand, FramePublication, InputCertainty, UiDevice, UiRuntime,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

struct FixedFrameDevice {
    frame: DynamicImage,
}

struct FailingCaptureDevice;

impl UiDevice for FailingCaptureDevice {
    fn capture(&mut self) -> Result<DynamicImage> {
        anyhow::bail!("game window is unavailable")
    }
}

impl UiDevice for FixedFrameDevice {
    fn capture(&mut self) -> Result<DynamicImage> {
        Ok(self.frame.clone())
    }
}

#[test]
fn caller_can_capture_a_frame_through_the_ui_runtime() {
    let runtime = UiRuntime::start(
        FixedFrameDevice {
            frame: DynamicImage::new_rgba8(7, 5),
        },
        2,
    )
    .expect("UI runtime should start");

    let operation = runtime
        .handle()
        .submit(CaptureFrame)
        .expect("capture should be accepted");
    let frame = operation
        .wait()
        .expect("runtime should answer")
        .expect("capture should succeed");

    assert_eq!(frame.image().dimensions(), (7, 5));
    runtime.shutdown().expect("UI runtime should stop");
}

#[test]
fn capture_failure_reports_that_no_input_was_sent() {
    let runtime = UiRuntime::start(FailingCaptureDevice, 1).expect("UI runtime should start");

    let failure = runtime
        .handle()
        .submit(CaptureFrame)
        .expect("capture should be accepted")
        .wait()
        .expect("runtime should answer")
        .expect_err("capture should fail");

    assert_eq!(failure.certainty(), InputCertainty::BeforeInput);
    assert_eq!(failure.stage(), "capture_frame");
    runtime.shutdown().expect("UI runtime should stop");
}

#[test]
fn declared_frame_demand_publishes_frames_and_the_latest_snapshot() {
    let runtime = UiRuntime::start(
        FixedFrameDevice {
            frame: DynamicImage::new_rgba8(11, 9),
        },
        2,
    )
    .expect("UI runtime should start");
    let handle = runtime.handle();

    let demand = handle
        .declare_frame_demand(FrameDemand::new(Duration::from_millis(10)).unwrap())
        .expect("frame demand should be accepted");
    let publication = demand
        .recv_timeout(Duration::from_secs(1))
        .expect("frame demand should publish");
    let FramePublication::Captured(published) = publication else {
        panic!("fixed frame device should publish a captured frame");
    };

    assert_eq!(published.image().dimensions(), (11, 9));
    let latest = handle
        .latest_frame()
        .expect("latest frame should be cached");
    assert_eq!(latest.image().dimensions(), (11, 9));
    assert_eq!(latest.captured_at(), published.captured_at());
    runtime.shutdown().expect("UI runtime should stop");
}

#[test]
fn declared_frame_demand_reports_capture_failure() {
    let runtime = UiRuntime::start(FailingCaptureDevice, 1).expect("UI runtime should start");
    let demand = runtime
        .handle()
        .declare_frame_demand(FrameDemand::new(Duration::from_millis(10)).unwrap())
        .expect("frame demand should be accepted");

    let publication = demand
        .recv_timeout(Duration::from_secs(1))
        .expect("frame demand should report a terminal capture result");
    let FramePublication::Failed(failure) = publication else {
        panic!("failing device should publish a capture failure");
    };

    assert!(failure.reason().contains("game window is unavailable"));
    runtime.shutdown().expect("UI runtime should stop");
}

struct OneShotCaptureDevice {
    captures: AtomicUsize,
}

impl UiDevice for OneShotCaptureDevice {
    fn capture(&mut self) -> Result<DynamicImage> {
        if self.captures.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok(DynamicImage::new_rgba8(13, 7))
        } else {
            anyhow::bail!("unexpected duplicate capture")
        }
    }
}

#[test]
fn capture_inside_a_ui_routine_satisfies_an_active_frame_demand() {
    let runtime = UiRuntime::start(
        OneShotCaptureDevice {
            captures: AtomicUsize::new(0),
        },
        2,
    )
    .expect("UI runtime should start");
    let handle = runtime.handle();
    let demand = handle
        .declare_frame_demand(FrameDemand::new(Duration::from_secs(1)).unwrap())
        .expect("frame demand should be accepted");

    let routine_frame = handle
        .submit(CaptureFrame)
        .expect("capture routine should be accepted")
        .wait()
        .expect("runtime should answer")
        .expect("the one available capture should succeed");
    let publication = demand
        .recv_timeout(Duration::from_millis(100))
        .expect("routine capture should satisfy the frame demand");
    let FramePublication::Captured(observed) = publication else {
        panic!("runtime performed a duplicate capture instead of reusing the routine frame");
    };

    assert_eq!(routine_frame.image().dimensions(), (13, 7));
    assert_eq!(observed.image().dimensions(), (13, 7));
    runtime.shutdown().expect("UI runtime should stop");
}
