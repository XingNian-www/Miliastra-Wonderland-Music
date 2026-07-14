use anyhow::Result;
use image::{DynamicImage, GenericImageView};
use miliastra_wonderland_music::runtime::ui::{CaptureFrame, InputCertainty, UiDevice, UiRuntime};

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
