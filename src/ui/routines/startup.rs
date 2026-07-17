use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::friend_delivery::{
    FriendDeliveryRoutineConfig, UiResidencyOutcome, UiResidencyTarget, before_input_failure,
    capture_normalized, restore_residency, sleep_ms,
};
use crate::adapters::windows::resolve_game_executable;
use crate::runtime::ocr::{OcrPriority, OcrRuntimeHandle};
use crate::runtime::ui::{
    InputCertainty, UiOperation, UiRoutine, UiRoutineContext, UiRoutineFailure, UiRuntimeHandle,
    UiSubmitError, sealed,
};
use crate::text::normalize_comparison_text as normalize_lock_text;
use crate::ui::change_detection::{change_stats, rect_chat_change_fingerprint};
use crate::ui::geometry::{Point, Rect, crop_canvas};
use crate::ui::state::{ResolvedUiTemplateArgs, detect_ui_state};
use crate::ui::template::best_template_hit;
use enigo::Key;

const ENTER_GAME_TEXT: &str = "点击进入";
const TEMPLATE_STABLE_HITS: u32 = 2;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct EnterGame;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct EnterWonderland;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EnterGameEffect {
    WindowReady,
    Entered,
    Failed(UiRoutineFailure),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EnterGameOutcome {
    effect: EnterGameEffect,
    residency: UiResidencyOutcome,
}

impl EnterGameOutcome {
    pub(crate) fn effect(&self) -> &EnterGameEffect {
        &self.effect
    }

    pub(crate) fn residency(&self) -> &UiResidencyOutcome {
        &self.residency
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EnterWonderlandEffect {
    Entered,
    Failed(UiRoutineFailure),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EnterWonderlandOutcome {
    effect: EnterWonderlandEffect,
    residency: UiResidencyOutcome,
}

impl EnterWonderlandOutcome {
    pub(crate) fn effect(&self) -> &EnterWonderlandEffect {
        &self.effect
    }

    pub(crate) fn residency(&self) -> &UiResidencyOutcome {
        &self.residency
    }
}

#[derive(Clone)]
pub(crate) struct StartupUi {
    runtime: UiRuntimeHandle,
    ocr: OcrRuntimeHandle,
    config: StartupRoutineConfig,
}

impl StartupUi {
    pub(crate) fn new(
        runtime: UiRuntimeHandle,
        ocr: OcrRuntimeHandle,
        config: StartupRoutineConfig,
    ) -> Self {
        Self {
            runtime,
            ocr,
            config,
        }
    }

    pub(crate) fn submit_enter_game(
        &self,
        request: EnterGame,
    ) -> Result<UiOperation<EnterGameOutcome>, UiSubmitError> {
        self.runtime.submit(EnterGameRoutine {
            request,
            ocr: self.ocr.clone(),
            config: self.config.clone(),
        })
    }

    pub(crate) fn submit_enter_wonderland(
        &self,
        request: EnterWonderland,
    ) -> Result<UiOperation<EnterWonderlandOutcome>, UiSubmitError> {
        self.runtime.submit(EnterWonderlandRoutine {
            request,
            ocr: self.ocr.clone(),
            config: self.config.clone(),
        })
    }
}

#[derive(Clone)]
pub(crate) struct StartupRoutineConfig {
    startup: StartupUiConfig,
    residency: FriendDeliveryRoutineConfig,
    templates: ResolvedUiTemplateArgs,
    target_process: String,
}

#[derive(Clone)]
pub(crate) struct StartupUiConfig {
    pub(crate) launch_game: bool,
    pub(crate) enter_game: bool,
    pub(crate) exe_path: PathBuf,
    pub(crate) game_args: String,
    pub(crate) launch_wait_ms: u64,
    pub(crate) launch_retries: u32,
    pub(crate) enter_game_timeout_ms: u64,
    pub(crate) enter_wonderland_timeout_ms: u64,
    pub(crate) wonderland_home_retries: u32,
    pub(crate) wonderland_home_retry_ms: u64,
    pub(crate) wonderland_card_retries: u32,
    pub(crate) wonderland_card_retry_ms: u64,
    pub(crate) wonderland_confirm_absent_timeout_ms: u64,
    pub(crate) wonderland_confirm_stable_timeout_ms: u64,
    pub(crate) final_primary_timeout_ms: u64,
    pub(crate) poll_ms: u64,
    pub(crate) stable_mean_threshold: f32,
    pub(crate) stable_changed_ratio_threshold: f32,
    pub(crate) template_threshold: f32,
    pub(crate) wonderland_enter_button_threshold: f32,
    pub(crate) templates: StartupUiTemplates,
    pub(crate) enter_game_text_region: Rect,
    pub(crate) wonderland_enter_button_region: Rect,
    pub(crate) main_ui_region: Rect,
    pub(crate) wonderland_close_region: Rect,
    pub(crate) wonderland_card_point: Point,
}

#[derive(Clone)]
pub(crate) struct StartupUiTemplates {
    pub(crate) wonderland_enter_button: PathBuf,
    pub(crate) paimon_menu: PathBuf,
    pub(crate) wonderland_close: PathBuf,
}

impl StartupRoutineConfig {
    pub(crate) fn resolve(
        startup: StartupUiConfig,
        residency: FriendDeliveryRoutineConfig,
        templates: ResolvedUiTemplateArgs,
        target_process: String,
    ) -> Self {
        Self {
            startup,
            residency,
            templates,
            target_process,
        }
    }
}

struct EnterGameRoutine {
    request: EnterGame,
    ocr: OcrRuntimeHandle,
    config: StartupRoutineConfig,
}

impl sealed::UiRoutineSealed for EnterGameRoutine {}

impl UiRoutine for EnterGameRoutine {
    type Output = EnterGameOutcome;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        let _ = self.request;
        let effect = match execute_enter_game(context, &self.ocr, &self.config) {
            Ok(effect) => effect,
            Err(failure) => EnterGameEffect::Failed(failure),
        };
        let residency = match &effect {
            EnterGameEffect::Entered => UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary),
            _ => observe_primary(context, &self.config),
        };
        EnterGameOutcome { effect, residency }
    }
}

struct EnterWonderlandRoutine {
    request: EnterWonderland,
    ocr: OcrRuntimeHandle,
    config: StartupRoutineConfig,
}

impl sealed::UiRoutineSealed for EnterWonderlandRoutine {}

impl UiRoutine for EnterWonderlandRoutine {
    type Output = EnterWonderlandOutcome;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        let _ = self.request;
        let mut goal_attempted = false;
        let effect =
            match execute_enter_wonderland(context, &self.ocr, &self.config, &mut goal_attempted) {
                Ok(()) => EnterWonderlandEffect::Entered,
                Err(failure) => EnterWonderlandEffect::Failed(failure),
            };
        let residency = wait_for_primary(
            context,
            &self.config,
            !goal_attempted,
            self.config.startup.final_primary_timeout_ms,
        );
        EnterWonderlandOutcome { effect, residency }
    }
}

fn execute_enter_game(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &StartupRoutineConfig,
) -> Result<EnterGameEffect, UiRoutineFailure> {
    ensure_game_window(context, config)?;
    context
        .device()
        .focus(config.residency.after_activate_ms)
        .map_err(|error| before_input_failure("focus_game_window", error))?;
    if !config.startup.enter_game {
        return Ok(EnterGameEffect::WindowReady);
    }

    let deadline = Instant::now() + Duration::from_millis(config.startup.enter_game_timeout_ms);
    let mut paimon_streak = 0_u32;
    let mut clicked = false;
    while Instant::now() < deadline {
        let image = capture_normalized(context, &config.residency, "observe_enter_game")?;
        if template_visible(
            &image,
            config.startup.main_ui_region,
            &config.startup.templates.paimon_menu,
            config.startup.template_threshold,
        )? {
            paimon_streak = paimon_streak.saturating_add(1);
            if paimon_streak >= TEMPLATE_STABLE_HITS {
                return Ok(EnterGameEffect::Entered);
            }
        } else {
            paimon_streak = 0;
        }

        if let Some(point) = find_enter_game_text(ocr, &image, config)? {
            context
                .device()
                .click_point(point.x, point.y)
                .map_err(|error| {
                    UiRoutineFailure::new(
                        InputCertainty::AfterInputUnknown,
                        "click_enter_game",
                        format!("{error:#}"),
                    )
                })?;
            clicked = true;
        }
        sleep_ms(config.startup.poll_ms);
    }
    Err(UiRoutineFailure::new(
        if clicked {
            InputCertainty::AfterInputUnknown
        } else {
            InputCertainty::ConfirmedFailure
        },
        "confirm_enter_game",
        "paimon menu template did not become stable before timeout",
    ))
}

fn ensure_game_window(
    context: &mut UiRoutineContext<'_>,
    config: &StartupRoutineConfig,
) -> Result<(), UiRoutineFailure> {
    if context.device().ensure_window().is_ok() {
        return Ok(());
    }
    if !config.startup.launch_game {
        return Err(UiRoutineFailure::new(
            InputCertainty::BeforeInput,
            "ensure_game_window",
            "game window is missing and startup.launch_game is false",
        ));
    }
    let executable = resolve_game_executable(&config.startup.exe_path, &config.target_process)
        .map_err(|error| before_input_failure("resolve_game_path", error))?;
    if !executable.exists() {
        return Err(UiRoutineFailure::new(
            InputCertainty::BeforeInput,
            "resolve_game_path",
            format!("game executable does not exist: {}", executable.display()),
        ));
    }
    let args = split_command_args(&config.startup.game_args)
        .map_err(|error| before_input_failure("parse_game_args", error))?;
    context
        .device()
        .launch_game(&executable, &args)
        .map_err(|error| {
            UiRoutineFailure::new(
                InputCertainty::AfterInputUnknown,
                "launch_game",
                format!("{error:#}"),
            )
        })?;
    for _ in 0..config.startup.launch_retries.max(1) {
        sleep_ms(config.startup.launch_wait_ms);
        if context.device().ensure_window().is_ok() {
            return Ok(());
        }
    }
    Err(UiRoutineFailure::new(
        InputCertainty::AfterInputUnknown,
        "wait_game_window",
        "launched game process but target window did not appear",
    ))
}

fn execute_enter_wonderland(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &StartupRoutineConfig,
    goal_attempted: &mut bool,
) -> Result<(), UiRoutineFailure> {
    context
        .device()
        .ensure_window()
        .map_err(|error| before_input_failure("ensure_wonderland_window", error))?;
    context
        .device()
        .focus(config.residency.after_activate_ms)
        .map_err(|error| before_input_failure("focus_wonderland_window", error))?;
    restore_residency(context, ocr, &config.residency, UiResidencyTarget::Primary)?;

    let home_attempts = capped_attempts(
        config.startup.wonderland_home_retries,
        config.startup.enter_wonderland_timeout_ms,
        config.startup.wonderland_home_retry_ms,
    );
    let mut home_ready = false;
    for _ in 0..home_attempts {
        context
            .device()
            .press_key(Key::F6)
            .map_err(|error| before_input_failure("open_wonderland_home", error))?;
        sleep_ms(config.startup.wonderland_home_retry_ms.max(100));
        if stable_template_visible(
            context,
            config,
            &config.startup.templates.wonderland_close,
            config.startup.wonderland_close_region,
            config.startup.template_threshold,
        )? {
            home_ready = true;
            break;
        }
    }
    if !home_ready {
        return Err(UiRoutineFailure::new(
            InputCertainty::ConfirmedFailure,
            "open_wonderland_home",
            "wonderland home template did not become stable",
        ));
    }

    let card_attempts = capped_attempts(
        config.startup.wonderland_card_retries,
        config.startup.enter_wonderland_timeout_ms,
        config.startup.wonderland_card_retry_ms,
    );
    for _ in 0..card_attempts {
        context
            .device()
            .click_point(
                config.startup.wonderland_card_point.x,
                config.startup.wonderland_card_point.y,
            )
            .map_err(|error| before_input_failure("select_wonderland_card", error))?;
        if let Some(point) = wait_template_hit(
            context,
            config,
            &config.startup.templates.wonderland_enter_button,
            config.startup.wonderland_enter_button_region,
            config.startup.wonderland_enter_button_threshold,
            config.startup.wonderland_card_retry_ms.max(100),
        )? {
            context
                .device()
                .click_point(point.x, point.y)
                .map_err(|error| {
                    UiRoutineFailure::new(
                        InputCertainty::AfterInputUnknown,
                        "confirm_enter_wonderland",
                        format!("{error:#}"),
                    )
                })?;
            *goal_attempted = true;
            return confirm_wonderland_transition(context, config);
        }
    }
    Err(UiRoutineFailure::new(
        InputCertainty::ConfirmedFailure,
        "locate_wonderland_confirmation",
        "wonderland confirmation template was not found",
    ))
}

fn confirm_wonderland_transition(
    context: &mut UiRoutineContext<'_>,
    config: &StartupRoutineConfig,
) -> Result<(), UiRoutineFailure> {
    let region = config.startup.wonderland_enter_button_region;
    let deadline =
        Instant::now() + Duration::from_millis(config.startup.wonderland_confirm_absent_timeout_ms);
    while Instant::now() < deadline {
        let image = capture_normalized(context, &config.residency, "confirm_wonderland_absent")?;
        if !template_visible(
            &image,
            region,
            &config.startup.templates.wonderland_enter_button,
            config.startup.wonderland_enter_button_threshold,
        )? {
            return wait_region_stable(context, config, region);
        }
        sleep_ms(config.startup.poll_ms);
    }
    Err(UiRoutineFailure::new(
        InputCertainty::AfterInputUnknown,
        "confirm_wonderland_absent",
        "wonderland confirmation template did not disappear",
    ))
}

fn wait_region_stable(
    context: &mut UiRoutineContext<'_>,
    config: &StartupRoutineConfig,
    region: Rect,
) -> Result<(), UiRoutineFailure> {
    let deadline =
        Instant::now() + Duration::from_millis(config.startup.wonderland_confirm_stable_timeout_ms);
    let image = capture_normalized(context, &config.residency, "observe_wonderland_transition")?;
    let mut previous = rect_chat_change_fingerprint(&image, region)
        .map_err(|error| before_input_failure("observe_wonderland_transition", error))?;
    while Instant::now() < deadline {
        sleep_ms(config.startup.poll_ms);
        let image = capture_normalized(context, &config.residency, "confirm_wonderland_stable")?;
        let current = rect_chat_change_fingerprint(&image, region)
            .map_err(|error| before_input_failure("confirm_wonderland_stable", error))?;
        let stats = change_stats(&previous, &current);
        if stats.mean_abs_diff <= config.startup.stable_mean_threshold
            && stats.changed_ratio <= config.startup.stable_changed_ratio_threshold
        {
            return Ok(());
        }
        previous = current;
    }
    Err(UiRoutineFailure::new(
        InputCertainty::AfterInputUnknown,
        "confirm_wonderland_stable",
        "wonderland transition region did not become stable",
    ))
}

fn wait_for_primary(
    context: &mut UiRoutineContext<'_>,
    config: &StartupRoutineConfig,
    allow_escape: bool,
    timeout_ms: u64,
) -> UiResidencyOutcome {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut streak = 0_u32;
    while Instant::now() < deadline {
        let image = match capture_normalized(context, &config.residency, "confirm_startup_primary")
        {
            Ok(image) => image,
            Err(failure) => return UiResidencyOutcome::Failed(failure),
        };
        let state = match detect_ui_state(&image, &config.templates, &config.residency.screen) {
            Ok(state) => state,
            Err(error) => {
                return UiResidencyOutcome::Failed(before_input_failure(
                    "confirm_startup_primary",
                    error,
                ));
            }
        };
        if state.is_primary() {
            streak = streak.saturating_add(1);
            if streak >= config.residency.stable_count {
                return UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary);
            }
        } else {
            streak = 0;
            if allow_escape
                && state.is_secondary()
                && let Err(error) = context.device().press_key(Key::Escape)
            {
                return UiResidencyOutcome::Failed(UiRoutineFailure::new(
                    InputCertainty::AfterInputUnknown,
                    "recover_startup_primary",
                    format!("{error:#}"),
                ));
            }
        }
        sleep_ms(config.startup.poll_ms);
    }
    UiResidencyOutcome::Failed(UiRoutineFailure::new(
        InputCertainty::ConfirmedFailure,
        "confirm_startup_primary",
        "primary UI did not become stable before timeout",
    ))
}

fn observe_primary(
    context: &mut UiRoutineContext<'_>,
    config: &StartupRoutineConfig,
) -> UiResidencyOutcome {
    let image = match capture_normalized(context, &config.residency, "observe_startup_residency") {
        Ok(image) => image,
        Err(failure) => return UiResidencyOutcome::Failed(failure),
    };
    match detect_ui_state(&image, &config.templates, &config.residency.screen) {
        Ok(state) if state.is_primary() => {
            UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary)
        }
        Ok(_) => UiResidencyOutcome::Failed(UiRoutineFailure::new(
            InputCertainty::ConfirmedFailure,
            "observe_startup_residency",
            "startup completed without a confirmed primary residency",
        )),
        Err(error) => {
            UiResidencyOutcome::Failed(before_input_failure("observe_startup_residency", error))
        }
    }
}

fn find_enter_game_text(
    ocr: &OcrRuntimeHandle,
    image: &image::DynamicImage,
    config: &StartupRoutineConfig,
) -> Result<Option<Point>, UiRoutineFailure> {
    let region = config.startup.enter_game_text_region;
    let crop = crop_canvas(image, region)
        .map_err(|error| before_input_failure("crop_enter_game_text", error))?;
    let target = normalize_lock_text(ENTER_GAME_TEXT);
    let lines = ocr
        .recognize_lines(crop, OcrPriority::UiConfirmation)
        .map_err(|error| before_input_failure("ocr_enter_game_text", error))?;
    Ok(lines.into_iter().find_map(|line| {
        let recognized = normalize_lock_text(&line.text);
        (recognized == target || recognized.contains(&target)).then(|| {
            Point::new(
                region.x + line.bbox.center().x,
                region.y + line.bbox.center().y,
            )
        })
    }))
}

fn stable_template_visible(
    context: &mut UiRoutineContext<'_>,
    config: &StartupRoutineConfig,
    template: &Path,
    region: Rect,
    threshold: f32,
) -> Result<bool, UiRoutineFailure> {
    for _ in 0..TEMPLATE_STABLE_HITS {
        let image = capture_normalized(context, &config.residency, "confirm_startup_template")?;
        if !template_visible(&image, region, template, threshold)? {
            return Ok(false);
        }
        sleep_ms(config.startup.poll_ms);
    }
    Ok(true)
}

fn wait_template_hit(
    context: &mut UiRoutineContext<'_>,
    config: &StartupRoutineConfig,
    template: &Path,
    region: Rect,
    threshold: f32,
    timeout_ms: u64,
) -> Result<Option<Point>, UiRoutineFailure> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        let image = capture_normalized(context, &config.residency, "locate_startup_template")?;
        if let Some(hit) = best_template_hit(&image, Some(region), template, threshold)
            .map_err(|error| before_input_failure("locate_startup_template", error))?
        {
            return Ok(Some(hit.center()));
        }
        sleep_ms(config.startup.poll_ms);
    }
    Ok(None)
}

fn template_visible(
    image: &image::DynamicImage,
    region: Rect,
    template: &Path,
    threshold: f32,
) -> Result<bool, UiRoutineFailure> {
    best_template_hit(image, Some(region), template, threshold)
        .map(|hit| hit.is_some())
        .map_err(|error| before_input_failure("match_startup_template", error))
}

fn split_command_args(value: &str) -> anyhow::Result<Vec<String>> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for ch in value.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ch if ch.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            ch => current.push(ch),
        }
    }
    if escaped {
        current.push('\\');
    }
    if quoted {
        anyhow::bail!("startup.game_args contains an unclosed quote");
    }
    if !current.is_empty() {
        args.push(current);
    }
    Ok(args)
}

fn capped_attempts(configured_retries: u32, timeout_ms: u64, interval_ms: u64) -> u32 {
    let interval_ms = interval_ms.max(1);
    let attempts = timeout_ms.max(interval_ms).div_ceil(interval_ms) as u32;
    configured_retries.max(1).min(attempts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_command_args_keeps_quoted_text() {
        assert_eq!(
            split_command_args(r#"-screen-fullscreen 0 "-window-title test""#).unwrap(),
            ["-screen-fullscreen", "0", "-window-title test"]
        );
    }

    #[test]
    fn split_command_args_rejects_unclosed_quote() {
        assert!(split_command_args(r#""abc"#).is_err());
    }
}
