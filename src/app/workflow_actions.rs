use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};

use super::config::{PointConfig, RectConfig, WindowConfig};
use super::geometry::{Point, Rect};
use super::input_actions::{
    activate_game, click_game_point, focus_game, hold_key, parse_key, paste_text, press_key,
};
use super::template_match::TemplateHit;
use super::ui_locator::{UiLocator, UiRegion};

#[derive(Clone, Copy, Debug)]
pub(super) enum TemplateMode {
    Present,
    Absent { stability: Option<PixelStability> },
}

#[derive(Clone, Copy, Debug)]
pub(super) struct PixelStability {
    pub(super) timeout_ms: u64,
    pub(super) mean_threshold: f32,
    pub(super) changed_ratio_threshold: f32,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum HitAction {
    Wait,
    Click { offset: PointConfig },
}

impl HitAction {
    fn click_offset(self) -> Option<PointConfig> {
        match self {
            Self::Wait => None,
            Self::Click { offset } => Some(offset),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Wait => "wait",
            Self::Click { .. } => "click",
        }
    }
}

pub(super) fn wait(wait_ms: u64) {
    let started = Instant::now();
    sleep(Duration::from_millis(wait_ms));
    log::info!(target: "timing",
        "原子动作耗时: action=wait total={}ms configured={}ms",
        elapsed_ms(started),
        wait_ms
    );
}

pub(super) fn press_key_text(key_text: &str, window_config: &WindowConfig) -> Result<()> {
    let started = Instant::now();
    let key_text = key_text.trim();
    if key_text.is_empty() {
        return Err(anyhow!("custom workflow step key is empty"));
    }
    let result = press_key(parse_key(key_text)?, window_config);
    log::info!(target: "timing",
        "原子动作耗时: action=press_key total={}ms success={}",
        elapsed_ms(started),
        result.is_ok()
    );
    result
}

pub(super) fn activate(window_config: &WindowConfig, after_activate_ms: u64) -> Result<()> {
    let started = Instant::now();
    let result = activate_game(window_config, after_activate_ms);
    log::info!(target: "timing",
        "原子动作耗时: action=activate_game total={}ms success={}",
        elapsed_ms(started),
        result.is_ok()
    );
    result
}

pub(super) fn focus(window_config: &WindowConfig, after_activate_ms: u64) -> Result<()> {
    let started = Instant::now();
    let result = focus_game(window_config, after_activate_ms);
    log::info!(target: "timing",
        "原子动作耗时: action=focus_game total={}ms success={}",
        elapsed_ms(started),
        result.is_ok()
    );
    result
}

pub(super) fn click_point(point: PointConfig, window_config: &WindowConfig) -> Result<()> {
    let started = Instant::now();
    let result = click_game_point(point, window_config);
    log::info!(target: "timing",
        "原子动作耗时: action=click_point total={}ms success={} x={} y={}",
        elapsed_ms(started),
        result.is_ok(),
        point.x,
        point.y
    );
    result
}

pub(super) fn paste(
    text: &str,
    window_config: &WindowConfig,
    clipboard_hold_ms: u64,
) -> Result<()> {
    let started = Instant::now();
    if text.is_empty() {
        return Err(anyhow!("custom workflow paste step missing text"));
    }
    let result = paste_text(text, window_config, clipboard_hold_ms);
    log::info!(target: "timing",
        "原子动作耗时: action=paste total={}ms success={} hold={}ms chars={}",
        elapsed_ms(started),
        result.is_ok(),
        clipboard_hold_ms,
        text.chars().count()
    );
    result
}

pub(super) fn hold_key_text<F>(
    key_text: &str,
    hold_seconds: u64,
    window_config: &WindowConfig,
    should_continue: F,
) -> Result<()>
where
    F: FnMut() -> bool,
{
    let started = Instant::now();
    let key_text = key_text.trim();
    if key_text.is_empty() {
        return Err(anyhow!("自定义流程按住按键缺少 key"));
    }
    let result = hold_key(
        parse_key(key_text)?,
        Duration::from_secs(hold_seconds),
        window_config,
        should_continue,
    );
    log::info!(target: "timing",
        "原子动作耗时: action=hold_key total={}ms configured={}ms success={}",
        elapsed_ms(started),
        hold_seconds.saturating_mul(1_000),
        result.is_ok()
    );
    result
}

pub(super) fn wait_pixels_stable<F>(
    locator: &UiLocator,
    region: RectConfig,
    stability: PixelStability,
    should_continue: F,
) -> Result<()>
where
    F: FnMut() -> bool,
{
    let started = Instant::now();
    let stable = locator.region(region.into()).wait_pixels_stable_while(
        stability.timeout_ms,
        stability.mean_threshold,
        stability.changed_ratio_threshold,
        should_continue,
    )?;
    log::info!(target: "timing",
        "原子动作耗时: action=wait_pixels_stable total={}ms timeout={}ms stable={} region={},{},{},{}",
        elapsed_ms(started),
        stability.timeout_ms,
        stable,
        region.x,
        region.y,
        region.width,
        region.height
    );
    if stable {
        Ok(())
    } else {
        Err(anyhow!("custom workflow pixels did not stabilize"))
    }
}

pub(super) fn locate_template<F>(
    locator: &UiLocator,
    template: &Path,
    region: RectConfig,
    threshold: f32,
    timeout_ms: u64,
    mode: TemplateMode,
    mut should_continue: F,
) -> Result<Option<TemplateHit>>
where
    F: FnMut() -> bool,
{
    let started = Instant::now();
    let region = locator.region(region.into());
    match mode {
        TemplateMode::Present => {
            let hit =
                region.wait_template_while(template, threshold, timeout_ms, should_continue)?;
            log::info!(target: "timing",
                "原子动作耗时: action=locate_template mode=present total={}ms timeout={}ms found={} template={}",
                elapsed_ms(started),
                timeout_ms,
                hit.is_some(),
                template.display()
            );
            Ok(hit)
        }
        TemplateMode::Absent { stability } => {
            let absent_started = Instant::now();
            if !region.wait_template_absent_while(
                template,
                threshold,
                timeout_ms,
                &mut should_continue,
            )? {
                log::info!(target: "timing",
                    "原子动作耗时: action=locate_template mode=absent total={}ms absent={} stable=false timeout={}ms template={}",
                    elapsed_ms(started),
                    false,
                    timeout_ms,
                    template.display()
                );
                return Err(anyhow!("custom workflow template still visible"));
            }
            let absent_ms = elapsed_ms(absent_started);
            let mut stable_ms = 0;
            if let Some(stability) = stability {
                let stable_started = Instant::now();
                if !region.wait_pixels_stable_while(
                    stability.timeout_ms,
                    stability.mean_threshold,
                    stability.changed_ratio_threshold,
                    should_continue,
                )? {
                    stable_ms = elapsed_ms(stable_started);
                    log::info!(target: "timing",
                        "原子动作耗时: action=locate_template mode=absent total={}ms absent=true absent_wait={}ms stable=false stable_wait={}ms timeout={}ms template={}",
                        elapsed_ms(started),
                        absent_ms,
                        stable_ms,
                        timeout_ms,
                        template.display()
                    );
                    return Err(anyhow!(
                        "custom workflow template disappeared but pixels did not stabilize"
                    ));
                }
                stable_ms = elapsed_ms(stable_started);
            }
            log::info!(target: "timing",
                "原子动作耗时: action=locate_template mode=absent total={}ms absent=true absent_wait={}ms stable={} stable_wait={}ms timeout={}ms template={}",
                elapsed_ms(started),
                absent_ms,
                stability.is_some(),
                stable_ms,
                timeout_ms,
                template.display()
            );
            Ok(None)
        }
    }
}

pub(super) fn wait_or_click_template<F>(
    locator: &UiLocator,
    template: &Path,
    region: RectConfig,
    threshold: f32,
    timeout_ms: u64,
    action: HitAction,
    should_continue: F,
) -> Result<Option<TemplateHit>>
where
    F: FnMut() -> bool,
{
    let started = Instant::now();
    let Some(hit) = locate_template(
        locator,
        template,
        region,
        threshold,
        timeout_ms,
        TemplateMode::Present,
        should_continue,
    )?
    else {
        log::info!(target: "timing",
            "原子动作耗时: action=wait_or_click_template total={}ms timeout={}ms hit=false clicked=false mode={}",
            elapsed_ms(started),
            timeout_ms,
            action.label()
        );
        return Ok(None);
    };
    let mut clicked = false;
    if let Some(offset) = action.click_offset() {
        let point = hit.center();
        locator.click_point(Point::new(point.x + offset.x, point.y + offset.y))?;
        clicked = true;
    }
    log::info!(target: "timing",
        "原子动作耗时: action=wait_or_click_template total={}ms timeout={}ms hit=true clicked={} mode={}",
        elapsed_ms(started),
        timeout_ms,
        clicked,
        action.label()
    );
    Ok(Some(hit))
}

pub(super) struct ScrollTemplateOptions {
    pub(super) max_scrolls: u32,
    pub(super) scroll_length: i32,
    pub(super) settle_ms: u64,
}

pub(super) fn click_scrollable_template<F>(
    locator: &UiLocator,
    template: &Path,
    search_region: Rect,
    scroll_region: Rect,
    threshold: f32,
    options: ScrollTemplateOptions,
    mut should_continue: F,
) -> Result<Option<TemplateHit>>
where
    F: FnMut() -> bool,
{
    let started = Instant::now();
    for attempt in 0..=options.max_scrolls {
        if !should_continue() {
            return Ok(None);
        }
        if let Some(hit) = locator
            .region(search_region)
            .find_template_with_threshold(template, threshold)?
        {
            locator.click_point(hit.center())?;
            log::info!(target: "timing",
                "原子动作耗时: action=click_scrollable_template total={}ms scrolls={} hit=true score={:.3} template={}",
                elapsed_ms(started),
                attempt,
                hit.score,
                template.display()
            );
            return Ok(Some(hit));
        }
        if attempt == options.max_scrolls {
            break;
        }
        locator.scroll_point(scroll_region.center(), options.scroll_length)?;
        wait(options.settle_ms);
    }
    log::info!(target: "timing",
        "原子动作耗时: action=click_scrollable_template total={}ms scrolls={} hit=false template={}",
        elapsed_ms(started),
        options.max_scrolls,
        template.display()
    );
    Ok(None)
}

pub(super) fn wait_or_click_text<F>(
    locator: &UiLocator,
    expected: &str,
    region: RectConfig,
    timeout_ms: u64,
    action: HitAction,
    mut should_continue: F,
    mut find_text: impl for<'a> FnMut(&UiRegion<'a>, &str) -> Result<Option<Point>>,
) -> Result<Option<Point>>
where
    F: FnMut() -> bool,
{
    if expected.is_empty() {
        return Err(anyhow!("custom workflow text step missing text"));
    }
    let started = Instant::now();
    let region = locator.region(region.into());
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut attempts = 0_u32;
    let mut ocr_ms = 0_u128;
    loop {
        if !should_continue() {
            log::info!(target: "timing",
                "原子动作耗时: action=wait_or_click_text total={}ms timeout={}ms attempts={} ocr={}ms hit=false clicked=false canceled=true mode={}",
                elapsed_ms(started),
                timeout_ms,
                attempts,
                ocr_ms,
                action.label()
            );
            return Ok(None);
        }
        let ocr_started = Instant::now();
        let found = find_text(&region, expected)?;
        ocr_ms += elapsed_ms(ocr_started);
        attempts += 1;
        if let Some(point) = found {
            let mut clicked = false;
            if let Some(offset) = action.click_offset() {
                locator.click_point(Point::new(point.x + offset.x, point.y + offset.y))?;
                clicked = true;
            }
            log::info!(target: "timing",
                "原子动作耗时: action=wait_or_click_text total={}ms timeout={}ms attempts={} ocr={}ms hit=true clicked={} canceled=false mode={}",
                elapsed_ms(started),
                timeout_ms,
                attempts,
                ocr_ms,
                clicked,
                action.label()
            );
            return Ok(Some(point));
        }
        if Instant::now() >= deadline {
            log::info!(target: "timing",
                "原子动作耗时: action=wait_or_click_text total={}ms timeout={}ms attempts={} ocr={}ms hit=false clicked=false canceled=false mode={}",
                elapsed_ms(started),
                timeout_ms,
                attempts,
                ocr_ms,
                action.label()
            );
            return Ok(None);
        }
        sleep(Duration::from_millis(locator.poll_ms()));
    }
}

fn elapsed_ms(started: Instant) -> u128 {
    started.elapsed().as_millis()
}
