use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::Result;
use ocr_rs::OcrEngine;

use super::FrameArgs;
use super::change_detection::{change_stats, rect_chat_change_fingerprint};
use super::command;
use super::config::{self, PointConfig};
use super::frame_source::{Canvas, Frame, load_frame};
use super::geometry::{Point, Rect, crop_canvas};
use super::input_actions::click_game_point;
use super::ocr::recognize_lines;
use super::template_match::{TemplateHit, best_template_hit};

pub(super) fn startup_locator(config: &config::AppConfig) -> UiLocator {
    startup_locator_with_poll_ms(config, config.startup.poll_ms)
}

pub(super) fn startup_transition_locator(config: &config::AppConfig) -> UiLocator {
    startup_locator_with_poll_ms(config, config.timing.input.click_ms.max(100))
}

fn startup_locator_with_poll_ms(config: &config::AppConfig, poll_ms: u64) -> UiLocator {
    UiLocator::new(
        Canvas {
            width: config.screen.expected_width,
            height: config.screen.expected_height,
            resize: true,
        },
        FrameArgs { image: None },
        config.window.clone(),
        poll_ms,
    )
}

pub(super) struct UiLocator {
    canvas: Canvas,
    frame_args: FrameArgs,
    window_config: config::WindowConfig,
    poll_ms: u64,
}

impl UiLocator {
    pub(super) fn new(
        canvas: Canvas,
        frame_args: FrameArgs,
        window_config: config::WindowConfig,
        poll_ms: u64,
    ) -> Self {
        Self {
            canvas,
            frame_args,
            window_config,
            poll_ms: poll_ms.max(50),
        }
    }

    pub(super) fn capture(&self) -> Result<Frame> {
        load_frame(&self.frame_args, &self.canvas, &self.window_config)
    }

    pub(super) fn region(&self, rect: Rect) -> UiRegion<'_> {
        UiRegion {
            locator: self,
            rect,
        }
    }

    pub(super) fn click_point(&self, point: Point) -> Result<()> {
        click_game_point(PointConfig::new(point.x, point.y), &self.window_config)
    }

    pub(super) fn poll_ms(&self) -> u64 {
        self.poll_ms
    }
}

pub(super) struct UiRegion<'a> {
    locator: &'a UiLocator,
    rect: Rect,
}

impl UiRegion<'_> {
    pub(super) fn find_template_with_threshold(
        &self,
        template: &Path,
        threshold: f32,
    ) -> Result<Option<TemplateHit>> {
        let frame = self.locator.capture()?;
        best_template_hit(&frame.image, Some(self.rect), template, threshold)
    }

    pub(super) fn wait_template_while<F>(
        &self,
        template: &Path,
        threshold: f32,
        timeout_ms: u64,
        mut should_continue: F,
    ) -> Result<Option<TemplateHit>>
    where
        F: FnMut() -> bool,
    {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if !should_continue() {
                return Ok(None);
            }
            if let Some(hit) = self.find_template_with_threshold(template, threshold)? {
                return Ok(Some(hit));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            sleep(Duration::from_millis(self.locator.poll_ms));
        }
    }

    pub(super) fn wait_template_absent_while<F>(
        &self,
        template: &Path,
        threshold: f32,
        timeout_ms: u64,
        mut should_continue: F,
    ) -> Result<bool>
    where
        F: FnMut() -> bool,
    {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if !should_continue() {
                return Ok(false);
            }
            if self
                .find_template_with_threshold(template, threshold)?
                .is_none()
            {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            sleep(Duration::from_millis(self.locator.poll_ms));
        }
    }

    pub(super) fn find_text(
        &self,
        engine: &OcrEngine,
        expected: &str,
    ) -> Result<Option<UiTextHit>> {
        self.find_any_text(engine, &[expected])
    }

    pub(super) fn find_text_hits(
        &self,
        engine: &OcrEngine,
        expected: &str,
    ) -> Result<Vec<UiTextHit>> {
        let target = command::normalize_lock_text(expected);
        if target.is_empty() {
            return Ok(Vec::new());
        }
        let frame = self.locator.capture()?;
        let crop = crop_canvas(&frame.image, self.rect)?;
        let mut hits = Vec::new();
        for line in recognize_lines(engine, &crop)? {
            let normalized = command::normalize_lock_text(&line.text);
            if normalized.is_empty() {
                continue;
            }
            if normalized == target || normalized.contains(&target) || target.contains(&normalized)
            {
                hits.push(UiTextHit {
                    rect: Rect::new(
                        self.rect.x + line.bbox.x,
                        self.rect.y + line.bbox.y,
                        line.bbox.width,
                        line.bbox.height,
                    ),
                });
            }
        }
        Ok(hits)
    }

    pub(super) fn find_any_text(
        &self,
        engine: &OcrEngine,
        expected: &[&str],
    ) -> Result<Option<UiTextHit>> {
        let targets = expected
            .iter()
            .map(|text| command::normalize_lock_text(text))
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>();
        if targets.is_empty() {
            return Ok(None);
        }

        let frame = self.locator.capture()?;
        let crop = crop_canvas(&frame.image, self.rect)?;
        let mut fallback = None;
        for line in recognize_lines(engine, &crop)? {
            let normalized = command::normalize_lock_text(&line.text);
            if normalized.is_empty() {
                continue;
            }
            let hit = UiTextHit {
                rect: Rect::new(
                    self.rect.x + line.bbox.x,
                    self.rect.y + line.bbox.y,
                    line.bbox.width,
                    line.bbox.height,
                ),
            };
            if targets.contains(&normalized) {
                return Ok(Some(hit));
            }
            if fallback.is_none()
                && targets
                    .iter()
                    .any(|target| normalized.contains(target) || target.contains(&normalized))
            {
                fallback = Some(hit);
            }
        }
        Ok(fallback)
    }

    pub(super) fn wait_pixels_stable_while<F>(
        &self,
        timeout_ms: u64,
        mean_threshold: f32,
        changed_ratio_threshold: f32,
        mut should_continue: F,
    ) -> Result<bool>
    where
        F: FnMut() -> bool,
    {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut previous = rect_chat_change_fingerprint(&self.locator.capture()?.image, self.rect)?;
        loop {
            if !should_continue() {
                return Ok(false);
            }
            sleep(Duration::from_millis(self.locator.poll_ms));
            let current = rect_chat_change_fingerprint(&self.locator.capture()?.image, self.rect)?;
            let stats = change_stats(&previous, &current);
            if stats.mean_abs_diff <= mean_threshold
                && stats.changed_ratio <= changed_ratio_threshold
            {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            previous = current;
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct UiTextHit {
    rect: Rect,
}

impl UiTextHit {
    pub(super) fn center(self) -> Point {
        self.rect.center()
    }
}
