use enigo::Key;

use super::friend_delivery::{
    FriendDeliveryRoutineConfig, UiResidencyOutcome, UiResidencyTarget, before_input_failure,
    capture_normalized, restore_residency, sleep_ms,
};
use crate::config::AppConfig;
use crate::runtime::ocr::{OcrPriority, OcrRuntimeHandle};
use crate::runtime::ui::{
    InputCertainty, UiOperation, UiRoutine, UiRoutineContext, UiRoutineFailure, UiRuntimeHandle,
    UiSubmitError, sealed,
};
use crate::text::normalize_comparison_text as normalize_lock_text;
use crate::ui::geometry::{Rect, crop_canvas};
use crate::ui::locator::{
    HALL_INFO_OCR_SAMPLES, HallInfo, HallInfoSample, display_or_empty, merge_hall_info_samples,
    parse_hall_remaining_minutes,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReadHallInfo;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DetectPublicHall;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ToggleMicrophone;

#[derive(Clone, Debug)]
pub(crate) enum ReadHallInfoEffect {
    Read(HallInfo),
    Failed(UiRoutineFailure),
}

#[derive(Clone, Debug)]
pub(crate) struct ReadHallInfoOutcome {
    effect: ReadHallInfoEffect,
    residency: UiResidencyOutcome,
}

impl ReadHallInfoOutcome {
    pub(crate) fn effect(&self) -> &ReadHallInfoEffect {
        &self.effect
    }

    pub(crate) fn residency(&self) -> &UiResidencyOutcome {
        &self.residency
    }
}

#[derive(Clone, Debug)]
pub(crate) enum DetectPublicHallEffect {
    Detected { is_public: bool, info: HallInfo },
    Failed(UiRoutineFailure),
}

#[derive(Clone, Debug)]
pub(crate) struct DetectPublicHallOutcome {
    effect: DetectPublicHallEffect,
    residency: UiResidencyOutcome,
}

impl DetectPublicHallOutcome {
    pub(crate) fn effect(&self) -> &DetectPublicHallEffect {
        &self.effect
    }

    pub(crate) fn residency(&self) -> &UiResidencyOutcome {
        &self.residency
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ToggleMicrophoneEffect {
    Toggled,
    Failed(UiRoutineFailure),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ToggleMicrophoneOutcome {
    effect: ToggleMicrophoneEffect,
    residency: UiResidencyOutcome,
}

impl ToggleMicrophoneOutcome {
    pub(crate) fn effect(&self) -> &ToggleMicrophoneEffect {
        &self.effect
    }

    pub(crate) fn residency(&self) -> &UiResidencyOutcome {
        &self.residency
    }
}

#[derive(Clone)]
pub(crate) struct HallUi {
    runtime: UiRuntimeHandle,
    ocr: OcrRuntimeHandle,
    config: HallRoutineConfig,
}

impl HallUi {
    pub(crate) fn new(runtime: UiRuntimeHandle, ocr: OcrRuntimeHandle, config: &AppConfig) -> Self {
        Self {
            runtime,
            ocr,
            config: HallRoutineConfig::from_app(config),
        }
    }

    pub(crate) fn submit_read(
        &self,
        request: ReadHallInfo,
    ) -> Result<UiOperation<ReadHallInfoOutcome>, UiSubmitError> {
        self.runtime.submit(ReadHallInfoRoutine {
            request,
            ocr: self.ocr.clone(),
            config: self.config.clone(),
        })
    }

    pub(crate) fn submit_detect(
        &self,
        request: DetectPublicHall,
    ) -> Result<UiOperation<DetectPublicHallOutcome>, UiSubmitError> {
        self.runtime.submit(DetectPublicHallRoutine {
            request,
            ocr: self.ocr.clone(),
            config: self.config.clone(),
        })
    }

    pub(crate) fn submit_microphone(
        &self,
        request: ToggleMicrophone,
    ) -> Result<UiOperation<ToggleMicrophoneOutcome>, UiSubmitError> {
        self.runtime.submit(ToggleMicrophoneRoutine {
            request,
            ocr: self.ocr.clone(),
            config: self.config.clone(),
        })
    }
}

#[derive(Clone)]
struct HallRoutineConfig {
    residency: FriendDeliveryRoutineConfig,
    hall_name_region: Rect,
    hall_time_region: Rect,
    page_settle_ms: u64,
    sample_interval_ms: u64,
    same_line_y_tolerance: i32,
}

impl HallRoutineConfig {
    fn from_app(config: &AppConfig) -> Self {
        Self {
            residency: FriendDeliveryRoutineConfig::from_app(config),
            hall_name_region: config.screen.hall_name_rect.into(),
            hall_time_region: config.screen.hall_time_rect.into(),
            page_settle_ms: config.timing.hall.page_settle_ms,
            sample_interval_ms: config.timing.hall.ocr_sample_interval_ms,
            same_line_y_tolerance: config.ocr.same_line_y_tolerance,
        }
    }
}

struct ReadHallInfoRoutine {
    request: ReadHallInfo,
    ocr: OcrRuntimeHandle,
    config: HallRoutineConfig,
}

impl sealed::UiRoutineSealed for ReadHallInfoRoutine {}

impl UiRoutine for ReadHallInfoRoutine {
    type Output = ReadHallInfoOutcome;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        let _ = self.request;
        let mut opened = false;
        let effect = match read_hall_info_transaction(context, &self.ocr, &self.config, &mut opened)
        {
            Ok(info) => ReadHallInfoEffect::Read(info),
            Err(failure) => ReadHallInfoEffect::Failed(failure),
        };
        let residency = finish_hall_page(context, &self.ocr, &self.config, opened);
        ReadHallInfoOutcome { effect, residency }
    }
}

struct DetectPublicHallRoutine {
    request: DetectPublicHall,
    ocr: OcrRuntimeHandle,
    config: HallRoutineConfig,
}

impl sealed::UiRoutineSealed for DetectPublicHallRoutine {}

impl UiRoutine for DetectPublicHallRoutine {
    type Output = DetectPublicHallOutcome;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        let _ = self.request;
        let mut opened = false;
        let effect = match read_hall_info_transaction(context, &self.ocr, &self.config, &mut opened)
        {
            Ok(info) => {
                let is_public = normalize_lock_text(&info.name) == normalize_lock_text("公共大厅");
                DetectPublicHallEffect::Detected { is_public, info }
            }
            Err(failure) => DetectPublicHallEffect::Failed(failure),
        };
        let residency = finish_hall_page(context, &self.ocr, &self.config, opened);
        DetectPublicHallOutcome { effect, residency }
    }
}

struct ToggleMicrophoneRoutine {
    request: ToggleMicrophone,
    ocr: OcrRuntimeHandle,
    config: HallRoutineConfig,
}

impl sealed::UiRoutineSealed for ToggleMicrophoneRoutine {}

impl UiRoutine for ToggleMicrophoneRoutine {
    type Output = ToggleMicrophoneOutcome;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        let _ = self.request;
        let effect = match prepare_primary(context, &self.ocr, &self.config) {
            Ok(()) => match context.device().press_key(Key::Unicode('n')) {
                Ok(()) => {
                    sleep_ms(100);
                    ToggleMicrophoneEffect::Toggled
                }
                Err(error) => ToggleMicrophoneEffect::Failed(UiRoutineFailure::new(
                    InputCertainty::AfterInputUnknown,
                    "toggle_microphone",
                    format!("{error:#}"),
                )),
            },
            Err(failure) => ToggleMicrophoneEffect::Failed(failure),
        };
        let residency = match restore_residency(
            context,
            &self.ocr,
            &self.config.residency,
            UiResidencyTarget::Primary,
        ) {
            Ok(()) => UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary),
            Err(failure) => UiResidencyOutcome::Failed(failure),
        };
        ToggleMicrophoneOutcome { effect, residency }
    }
}

fn prepare_primary(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &HallRoutineConfig,
) -> Result<(), UiRoutineFailure> {
    context
        .device()
        .ensure_ready(config.residency.after_activate_ms)
        .map_err(|error| before_input_failure("prepare_hall_operation", error))?;
    restore_residency(context, ocr, &config.residency, UiResidencyTarget::Primary)
}

fn read_hall_info_transaction(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &HallRoutineConfig,
    opened: &mut bool,
) -> Result<HallInfo, UiRoutineFailure> {
    prepare_primary(context, ocr, config)?;
    context.device().press_key(Key::F2).map_err(|error| {
        UiRoutineFailure::new(
            InputCertainty::AfterInputUnknown,
            "open_hall_page",
            format!("{error:#}"),
        )
    })?;
    *opened = true;
    sleep_ms(config.page_settle_ms);

    let mut samples = Vec::with_capacity(HALL_INFO_OCR_SAMPLES);
    for index in 0..HALL_INFO_OCR_SAMPLES {
        if index > 0 {
            sleep_ms(config.sample_interval_ms);
        }
        let image = capture_normalized(context, &config.residency, "capture_hall_info")?;
        let sample = read_hall_sample(ocr, &image, config)?;
        log::info!(
            "大厅检测 OCR 采样: {}/{} name={} time={} minutes={}",
            index + 1,
            HALL_INFO_OCR_SAMPLES,
            display_or_empty(&sample.name),
            display_or_empty(&sample.time_text),
            sample
                .remaining_minutes
                .map(|minutes| minutes.to_string())
                .unwrap_or_else(|| "未知".to_string())
        );
        samples.push(sample);
    }
    Ok(merge_hall_info_samples(&samples))
}

fn read_hall_sample(
    ocr: &OcrRuntimeHandle,
    image: &image::DynamicImage,
    config: &HallRoutineConfig,
) -> Result<HallInfoSample, UiRoutineFailure> {
    let name_crop = crop_canvas(image, config.hall_name_region)
        .map_err(|error| before_input_failure("crop_hall_name", error))?;
    let name = ocr
        .merged_text(
            name_crop,
            config.same_line_y_tolerance,
            OcrPriority::UiConfirmation,
        )
        .map_err(|error| before_input_failure("ocr_hall_name", error))?;
    let time_crop = crop_canvas(image, config.hall_time_region)
        .map_err(|error| before_input_failure("crop_hall_time", error))?;
    let time_text = ocr
        .merged_text(
            time_crop,
            config.same_line_y_tolerance,
            OcrPriority::UiConfirmation,
        )
        .map_err(|error| before_input_failure("ocr_hall_time", error))?;
    Ok(HallInfoSample {
        name,
        remaining_minutes: parse_hall_remaining_minutes(&time_text),
        time_text,
    })
}

fn finish_hall_page(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &HallRoutineConfig,
    opened: bool,
) -> UiResidencyOutcome {
    if opened {
        if let Err(error) = context.device().press_key(Key::Escape) {
            return UiResidencyOutcome::Failed(UiRoutineFailure::new(
                InputCertainty::AfterInputUnknown,
                "close_hall_page",
                format!("{error:#}"),
            ));
        }
        sleep_ms(config.residency.click_ms);
    }
    match restore_residency(context, ocr, &config.residency, UiResidencyTarget::Primary) {
        Ok(()) => UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary),
        Err(failure) => UiResidencyOutcome::Failed(failure),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use anyhow::Result;
    use image::{DynamicImage, GenericImage};

    use super::*;
    use crate::runtime::ocr::{OcrDevice, OcrLine, OcrRuntime};
    use crate::runtime::ui::{UiDevice, UiRuntime};
    use crate::ui::geometry::Rect;

    struct HallDevice {
        frame: DynamicImage,
        keys: Arc<Mutex<Vec<Key>>>,
    }

    impl UiDevice for HallDevice {
        fn capture(&mut self) -> Result<DynamicImage> {
            Ok(self.frame.clone())
        }

        fn ensure_ready(&mut self, _after_activate_ms: u64) -> Result<()> {
            Ok(())
        }

        fn press_key(&mut self, key: Key) -> Result<()> {
            self.keys.lock().unwrap().push(key);
            Ok(())
        }
    }

    struct HallOcrDevice {
        calls: usize,
    }

    impl OcrDevice for HallOcrDevice {
        fn recognize_lines(&mut self, image: &DynamicImage) -> Result<Vec<OcrLine>> {
            let text = if self.calls.is_multiple_of(2) {
                "公共大厅"
            } else {
                ""
            };
            self.calls += 1;
            Ok(vec![OcrLine {
                text: text.to_string(),
                confidence: 1.0,
                bbox: Rect::new(0, 0, image.width(), image.height()),
            }])
        }
    }

    #[test]
    fn public_hall_detection_owns_f2_ocr_and_primary_recovery_in_one_operation() {
        let mut config = AppConfig::load(Path::new("config.yaml")).unwrap();
        config.timing.input.after_activate_ms = 0;
        config.timing.input.click_ms = 0;
        config.timing.hall.page_settle_ms = 0;
        config.timing.hall.ocr_sample_interval_ms = 0;
        config.timing.workflow.default_timeout_ms = 20;
        config.timing.workflow.default_poll_ms = 1;
        let keys = Arc::new(Mutex::new(Vec::new()));
        let ui_runtime = UiRuntime::start(
            HallDevice {
                frame: primary_frame(&config),
                keys: keys.clone(),
            },
            2,
        )
        .unwrap();
        let ocr_runtime = OcrRuntime::start(HallOcrDevice { calls: 0 }, 4).unwrap();
        let hall_ui = HallUi::new(ui_runtime.handle(), ocr_runtime.handle(), &config);

        let outcome = hall_ui
            .submit_detect(DetectPublicHall)
            .unwrap()
            .wait()
            .unwrap();

        assert!(matches!(
            outcome.effect(),
            DetectPublicHallEffect::Detected {
                is_public: true,
                ..
            }
        ));
        assert!(matches!(
            outcome.residency(),
            UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary)
        ));
        assert_eq!(*keys.lock().unwrap(), [Key::F2, Key::Escape]);

        ui_runtime.shutdown().unwrap();
        ocr_runtime.shutdown().unwrap();
    }

    fn primary_frame(config: &AppConfig) -> DynamicImage {
        let mut frame =
            DynamicImage::new_rgba8(config.screen.expected_width, config.screen.expected_height);
        let friend = image::open(&config.templates.friend).unwrap();
        frame
            .copy_from(
                &friend,
                config.screen.friend_rect.x as u32,
                config.screen.friend_rect.y as u32,
            )
            .unwrap();
        frame
    }
}
