use enigo::Key;
use std::sync::Arc;

use super::friend_delivery::{
    FriendDeliveryRoutineConfig, UiResidencyOutcome, UiResidencyTarget, after_input_failure,
    before_input_failure, capture_normalized, confirm_primary_residency, friend_list_drag_points,
    restore_residency, sleep_ms,
};
#[cfg(test)]
use crate::config::AppConfig;
use crate::config::{HallTimingConfig, OcrConfig, ScreenConfig};
use crate::runtime::ocr::{OcrPriority, OcrRuntimeHandle};
use crate::runtime::ui::{
    InputCertainty, UiOperation, UiRoutine, UiRoutineContext, UiRoutineFailure, UiRuntimeHandle,
    UiSubmitError, sealed,
};
use crate::text::normalize_comparison_text as normalize_lock_text;
use crate::ui::geometry::{Rect, crop_canvas};
use crate::ui::locator::{
    HALL_INFO_OCR_SAMPLES, HallInfo, HallInfoSample, display_or_empty, merge_hall_info_samples,
    parse_hall_member_count, parse_hall_remaining_minutes,
};

const HALL_MEMBER_SCROLL_THRESHOLD: u32 = 7;

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
    screenshot: Option<Arc<image::DynamicImage>>,
}

impl ReadHallInfoOutcome {
    pub(crate) fn effect(&self) -> &ReadHallInfoEffect {
        &self.effect
    }

    pub(crate) fn residency(&self) -> &UiResidencyOutcome {
        &self.residency
    }

    pub(crate) fn screenshot(&self) -> Option<Arc<image::DynamicImage>> {
        self.screenshot.clone()
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
    pub(crate) fn new(
        runtime: UiRuntimeHandle,
        ocr: OcrRuntimeHandle,
        config: HallRoutineConfig,
    ) -> Self {
        Self {
            runtime,
            ocr,
            config,
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
pub(crate) struct HallRoutineConfig {
    residency: FriendDeliveryRoutineConfig,
    hall_name_region: Rect,
    hall_member_count_region: Rect,
    hall_time_region: Rect,
    hall_member_list_region: Rect,
    page_settle_ms: u64,
    sample_interval_ms: u64,
    same_line_y_tolerance: i32,
}

impl HallRoutineConfig {
    pub(crate) fn resolve(
        residency: FriendDeliveryRoutineConfig,
        screen: &ScreenConfig,
        timing: &HallTimingConfig,
        ocr: &OcrConfig,
    ) -> Self {
        Self {
            residency,
            hall_name_region: screen.hall_name_rect.into(),
            hall_member_count_region: screen.hall_member_count_rect.into(),
            hall_time_region: screen.hall_time_rect.into(),
            hall_member_list_region: screen.hall_member_list_rect.into(),
            page_settle_ms: timing.page_settle_ms,
            sample_interval_ms: timing.ocr_sample_interval_ms,
            same_line_y_tolerance: ocr.same_line_y_tolerance,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_app(config: &AppConfig) -> Self {
        Self::resolve(
            FriendDeliveryRoutineConfig::from_app(config),
            &config.screen,
            &config.timing.hall,
            &config.ocr,
        )
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
        let (effect, screenshot) =
            match read_hall_info_transaction(context, &self.ocr, &self.config, &mut opened) {
                Ok((info, screenshot)) => (ReadHallInfoEffect::Read(info), Some(screenshot)),
                Err(failure) => (ReadHallInfoEffect::Failed(failure), None),
            };
        let residency = finish_hall_page(context, &self.ocr, &self.config, opened);
        ReadHallInfoOutcome {
            effect,
            residency,
            screenshot,
        }
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
            Ok((info, _screenshot)) => {
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
) -> Result<(HallInfo, Arc<image::DynamicImage>), UiRoutineFailure> {
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

    let initial_image = capture_normalized(
        context,
        &config.residency,
        "capture_hall_screenshot",
        InputCertainty::AfterInputUnknown,
    )?;
    let mut detection_image = initial_image.clone();
    let mut screenshot = initial_image.clone();
    match read_hall_member_count(ocr, &initial_image, config) {
        Ok(Some(member_count)) => {
            log::info!(
                "大厅成员人数 OCR 结果: {}，滚动阈值={}",
                member_count,
                HALL_MEMBER_SCROLL_THRESHOLD
            );
            if member_count > HALL_MEMBER_SCROLL_THRESHOLD {
                let (from, to) = friend_list_drag_points(config.hall_member_list_region);
                context
                    .device()
                    .drag_point(from.x, from.y, to.x, to.y)
                    .map_err(|error| after_input_failure("drag_hall_member_list", error))?;
                sleep_ms(config.page_settle_ms);
                let top_image = capture_normalized(
                    context,
                    &config.residency,
                    "capture_top_hall_screenshot",
                    InputCertainty::AfterInputUnknown,
                );
                context
                    .device()
                    .drag_point(to.x, to.y, from.x, from.y)
                    .map_err(|error| after_input_failure("restore_hall_member_list", error))?;
                sleep_ms(config.page_settle_ms);
                let bottom_image = capture_normalized(
                    context,
                    &config.residency,
                    "capture_bottom_hall_screenshot",
                    InputCertainty::AfterInputUnknown,
                )?;
                let top_image = top_image?;
                detection_image = bottom_image.clone();
                screenshot = merge_hall_screenshots(&top_image, &bottom_image);
            }
        }
        Ok(None) => log::debug!("大厅成员人数 OCR 未识别，跳过成员列表滚动"),
        Err(failure) => log::warn!("大厅成员人数 OCR 失败，跳过成员列表滚动: {failure}"),
    }
    let screenshot = Arc::new(screenshot);
    let mut samples = Vec::with_capacity(HALL_INFO_OCR_SAMPLES);
    for index in 0..HALL_INFO_OCR_SAMPLES {
        if index > 0 {
            sleep_ms(config.sample_interval_ms);
        }
        let image = if index == 0 {
            &detection_image
        } else {
            detection_image = capture_normalized(
                context,
                &config.residency,
                "capture_hall_info",
                InputCertainty::AfterInputUnknown,
            )?;
            &detection_image
        };
        let sample = read_hall_sample(ocr, image, config)?;
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
    Ok((merge_hall_info_samples(&samples), screenshot))
}

fn read_hall_member_count(
    ocr: &OcrRuntimeHandle,
    image: &image::DynamicImage,
    config: &HallRoutineConfig,
) -> Result<Option<u32>, UiRoutineFailure> {
    let count_crop = crop_canvas(image, config.hall_member_count_region)
        .map_err(|error| after_input_failure("crop_hall_member_count", error))?;
    let count_text = ocr
        .merged_text(
            count_crop,
            config.same_line_y_tolerance,
            OcrPriority::UiConfirmation,
        )
        .map_err(|error| after_input_failure("ocr_hall_member_count", error.into()))?;
    Ok(parse_hall_member_count(&count_text))
}

fn merge_hall_screenshots(
    first: &image::DynamicImage,
    scrolled: &image::DynamicImage,
) -> image::DynamicImage {
    let first = first.to_rgba8();
    let scrolled = scrolled.to_rgba8();
    let width = first.width().max(scrolled.width());
    let height = first.height().saturating_add(scrolled.height());
    let mut merged = image::RgbaImage::new(width, height);
    image::imageops::overlay(&mut merged, &first, 0, 0);
    image::imageops::overlay(&mut merged, &scrolled, 0, i64::from(first.height()));
    image::DynamicImage::ImageRgba8(merged)
}

fn read_hall_sample(
    ocr: &OcrRuntimeHandle,
    image: &image::DynamicImage,
    config: &HallRoutineConfig,
) -> Result<HallInfoSample, UiRoutineFailure> {
    let name_crop = crop_canvas(image, config.hall_name_region)
        .map_err(|error| after_input_failure("crop_hall_name", error))?;
    let name = ocr
        .merged_text(
            name_crop,
            config.same_line_y_tolerance,
            OcrPriority::UiConfirmation,
        )
        .map_err(|error| after_input_failure("ocr_hall_name", error.into()))?;
    let time_crop = crop_canvas(image, config.hall_time_region)
        .map_err(|error| after_input_failure("crop_hall_time", error))?;
    let time_text = ocr
        .merged_text(
            time_crop,
            config.same_line_y_tolerance,
            OcrPriority::UiConfirmation,
        )
        .map_err(|error| after_input_failure("ocr_hall_time", error.into()))?;
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
        return match confirm_primary_residency(context, &config.residency) {
            Ok(()) => UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary),
            Err(failure) => UiResidencyOutcome::Failed(failure),
        };
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
    use image::{DynamicImage, GenericImage, Rgba};

    use super::*;
    use crate::runtime::ocr::{OcrDevice, OcrLine, OcrRuntime};
    use crate::runtime::ui::{UiDevice, UiRuntime};
    use crate::ui::geometry::Rect;
    use crate::ui::state::{TemplateUiStateClassifier, UiTemplateArgs};

    type DragRecord = (i32, i32, i32, i32);

    struct HallDevice {
        frame: DynamicImage,
        keys: Arc<Mutex<Vec<Key>>>,
        drags: Arc<Mutex<Vec<DragRecord>>>,
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

        fn drag_point(&mut self, from_x: i32, from_y: i32, to_x: i32, to_y: i32) -> Result<()> {
            self.drags
                .lock()
                .unwrap()
                .push((from_x, from_y, to_x, to_y));
            let color = if from_y > to_y {
                Rgba([255, 0, 0, 255])
            } else {
                Rgba([0, 255, 0, 255])
            };
            self.frame.put_pixel(0, 0, color);
            Ok(())
        }
    }

    struct TransitionHallDevice {
        primary: DynamicImage,
        keys: Arc<Mutex<Vec<Key>>>,
        captures_after_escape: usize,
    }

    impl UiDevice for TransitionHallDevice {
        fn capture(&mut self) -> Result<DynamicImage> {
            let last_key = self.keys.lock().unwrap().last().cloned();
            match last_key {
                None => Ok(self.primary.clone()),
                Some(Key::Escape) => {
                    self.captures_after_escape += 1;
                    if self.captures_after_escape <= 2 {
                        Ok(DynamicImage::new_rgba8(
                            self.primary.width(),
                            self.primary.height(),
                        ))
                    } else {
                        Ok(self.primary.clone())
                    }
                }
                Some(_) => Ok(DynamicImage::new_rgba8(
                    self.primary.width(),
                    self.primary.height(),
                )),
            }
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
        member_count: &'static str,
    }

    fn start_test_ui_runtime(device: impl UiDevice, config: &AppConfig) -> UiRuntime {
        UiRuntime::start_with_state_classifier(
            device,
            2,
            TemplateUiStateClassifier::new(
                UiTemplateArgs::default().resolve(&config.templates, &config.ocr),
                config.screen.clone(),
            ),
            config.resolve_stability_count(0),
        )
        .unwrap()
    }

    impl OcrDevice for HallOcrDevice {
        fn recognize_lines(&mut self, image: &DynamicImage) -> Result<Vec<OcrLine>> {
            let text = match (image.width(), image.height()) {
                (450, 50) => self.member_count,
                (325, 40) => "公共大厅",
                _ => "",
            };
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
        config.timing.workflow.default_timeout_ms = 200;
        config.timing.workflow.default_poll_ms = 1;
        let keys = Arc::new(Mutex::new(Vec::new()));
        let drags = Arc::new(Mutex::new(Vec::new()));
        let ui_runtime = start_test_ui_runtime(
            HallDevice {
                frame: primary_frame(&config),
                keys: keys.clone(),
                drags: drags.clone(),
            },
            &config,
        );
        let ocr_runtime = OcrRuntime::start(
            HallOcrDevice {
                member_count: "大厅人数 8/12",
            },
            4,
        )
        .unwrap();
        let hall_ui = HallUi::new(
            ui_runtime.handle(),
            ocr_runtime.handle(),
            HallRoutineConfig::from_app(&config),
        );

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
        assert!(
            matches!(
                outcome.residency(),
                UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary)
            ),
            "{:?}",
            outcome.residency()
        );
        assert_eq!(*keys.lock().unwrap(), [Key::F2, Key::Escape]);
        assert_eq!(
            *drags.lock().unwrap(),
            [(1240, 910, 1240, 160), (1240, 160, 1240, 910)]
        );

        ui_runtime.shutdown().unwrap();
        ocr_runtime.shutdown().unwrap();
    }

    #[test]
    fn hall_screenshot_captures_top_then_bottom_after_scroll() {
        let mut config = AppConfig::load(Path::new("config.yaml")).unwrap();
        config.timing.input.after_activate_ms = 0;
        config.timing.input.click_ms = 0;
        config.timing.hall.page_settle_ms = 0;
        config.timing.hall.ocr_sample_interval_ms = 0;
        config.timing.workflow.default_timeout_ms = 200;
        config.timing.workflow.default_poll_ms = 1;
        let keys = Arc::new(Mutex::new(Vec::new()));
        let drags = Arc::new(Mutex::new(Vec::new()));
        let ui_runtime = start_test_ui_runtime(
            HallDevice {
                frame: primary_frame(&config),
                keys,
                drags: drags.clone(),
            },
            &config,
        );
        let ocr_runtime = OcrRuntime::start(
            HallOcrDevice {
                member_count: "大厅人数 8/12",
            },
            4,
        )
        .unwrap();
        let hall_ui = HallUi::new(
            ui_runtime.handle(),
            ocr_runtime.handle(),
            HallRoutineConfig::from_app(&config),
        );

        let outcome = hall_ui.submit_read(ReadHallInfo).unwrap().wait().unwrap();
        let screenshot = outcome.screenshot().expect("hall screenshot").to_rgba8();

        assert_eq!(*screenshot.get_pixel(0, 0), Rgba([255, 0, 0, 255]));
        assert_eq!(
            *screenshot.get_pixel(0, config.screen.expected_height),
            Rgba([0, 255, 0, 255])
        );
        assert_eq!(
            *drags.lock().unwrap(),
            [(1240, 910, 1240, 160), (1240, 160, 1240, 910)]
        );

        ui_runtime.shutdown().unwrap();
        ocr_runtime.shutdown().unwrap();
    }

    #[test]
    fn hall_exit_treats_unknown_frames_as_transition_until_primary_is_stable() {
        let mut config = AppConfig::load(Path::new("config.yaml")).unwrap();
        config.timing.input.after_activate_ms = 0;
        config.timing.input.click_ms = 0;
        config.timing.hall.page_settle_ms = 0;
        config.timing.hall.ocr_sample_interval_ms = 0;
        config.timing.workflow.default_timeout_ms = 300;
        config.timing.workflow.default_poll_ms = 1;
        let keys = Arc::new(Mutex::new(Vec::new()));
        let ui_runtime = start_test_ui_runtime(
            TransitionHallDevice {
                primary: primary_frame(&config),
                keys: keys.clone(),
                captures_after_escape: 0,
            },
            &config,
        );
        let ocr_runtime = OcrRuntime::start(
            HallOcrDevice {
                member_count: "大厅人数 3/12",
            },
            4,
        )
        .unwrap();
        let hall_ui = HallUi::new(
            ui_runtime.handle(),
            ocr_runtime.handle(),
            HallRoutineConfig::from_app(&config),
        );

        let outcome = hall_ui
            .submit_detect(DetectPublicHall)
            .unwrap()
            .wait()
            .unwrap();

        assert!(matches!(
            outcome.effect(),
            DetectPublicHallEffect::Detected { .. }
        ));
        assert!(
            matches!(
                outcome.residency(),
                UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary)
            ),
            "{:?}",
            outcome.residency()
        );
        assert_eq!(*keys.lock().unwrap(), [Key::F2, Key::Escape]);

        ui_runtime.shutdown().unwrap();
        ocr_runtime.shutdown().unwrap();
    }

    #[test]
    fn hall_screenshot_merge_stacks_before_and_after_scroll_frames() {
        let mut first = DynamicImage::new_rgba8(3, 2);
        first.put_pixel(0, 0, Rgba([255, 0, 0, 255]));
        let mut scrolled = DynamicImage::new_rgba8(3, 4);
        scrolled.put_pixel(0, 0, Rgba([0, 255, 0, 255]));

        let merged = merge_hall_screenshots(&first, &scrolled);
        let merged = merged.to_rgba8();

        assert_eq!((merged.width(), merged.height()), (3, 6));
        assert_eq!(*merged.get_pixel(0, 0), Rgba([255, 0, 0, 255]));
        assert_eq!(*merged.get_pixel(0, 2), Rgba([0, 255, 0, 255]));
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
