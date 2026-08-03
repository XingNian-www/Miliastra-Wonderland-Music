use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::friend_delivery::{
    FriendDeliveryRoutineConfig, UiResidencyOutcome, UiResidencyTarget, after_input_failure,
    before_input_failure, capture_normalized, sleep_ms,
};
use super::state_observation::wait_for_stable_ui_kind;
use crate::adapters::windows::resolve_game_executable;
use crate::runtime::ocr::{OcrPriority, OcrRuntimeHandle};
use crate::runtime::ui::{
    InputCertainty, UiOperation, UiRoutine, UiRoutineContext, UiRoutineFailure, UiRuntimeHandle,
    UiStateKind, UiSubmitError, sealed,
};
use crate::text::normalize_comparison_text as normalize_lock_text;
use crate::ui::change_detection::{change_stats, rect_chat_change_fingerprint};
use crate::ui::geometry::{Point, Rect, crop_canvas};
use crate::ui::template::best_template_hit;
use enigo::Key;

const ENTER_GAME_TEXT: &str = "点击进入";
const TEMPLATE_STABLE_HITS: u32 = 2;

struct TemplateAbsence<'a> {
    template: &'a Path,
    region: Rect,
    threshold: f32,
    timeout_ms: u64,
    stage: &'static str,
    failure_message: &'static str,
}

struct TemplateHit<'a> {
    template: &'a Path,
    region: Rect,
    threshold: f32,
    timeout_ms: u64,
    poll_ms: u64,
    certainty: InputCertainty,
}

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
    pub(crate) wonderland_map_star_retries: u32,
    pub(crate) wonderland_map_star_retry_ms: u64,
    pub(crate) wonderland_hall_retries: u32,
    pub(crate) wonderland_hall_retry_ms: u64,
    pub(crate) wonderland_transition_timeout_ms: u64,
    pub(crate) wonderland_confirm_stable_timeout_ms: u64,
    pub(crate) final_primary_timeout_ms: u64,
    pub(crate) poll_ms: u64,
    pub(crate) stable_mean_threshold: f32,
    pub(crate) stable_changed_ratio_threshold: f32,
    pub(crate) template_threshold: f32,
    pub(crate) wonderland_confirm_threshold: f32,
    pub(crate) templates: StartupUiTemplates,
    pub(crate) enter_game_text_region: Rect,
    pub(crate) wonderland_hall_ocr_region: Rect,
    pub(crate) wonderland_confirm_region: Rect,
    pub(crate) main_ui_region: Rect,
    pub(crate) wonderland_map_star_region: Rect,
}

#[derive(Clone)]
pub(crate) struct StartupUiTemplates {
    pub(crate) wonderland_map_star: PathBuf,
    pub(crate) wonderland_confirm: PathBuf,
    pub(crate) paimon_menu: PathBuf,
}

impl StartupRoutineConfig {
    pub(crate) fn resolve(
        startup: StartupUiConfig,
        residency: FriendDeliveryRoutineConfig,
        target_process: String,
    ) -> Self {
        Self {
            startup,
            residency,
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
        let image = capture_normalized(
            context,
            &config.residency,
            "observe_enter_game",
            if clicked {
                InputCertainty::AfterInputUnknown
            } else {
                InputCertainty::BeforeInput
            },
        )?;
        if template_visible(
            &image,
            config.startup.main_ui_region,
            &config.startup.templates.paimon_menu,
            config.startup.template_threshold,
            if clicked {
                InputCertainty::AfterInputUnknown
            } else {
                InputCertainty::BeforeInput
            },
        )? {
            paimon_streak = paimon_streak.saturating_add(1);
            if paimon_streak >= TEMPLATE_STABLE_HITS {
                return Ok(EnterGameEffect::Entered);
            }
        } else {
            paimon_streak = 0;
        }

        if let Some(point) = find_enter_game_text(
            ocr,
            &image,
            config,
            if clicked {
                InputCertainty::AfterInputUnknown
            } else {
                InputCertainty::BeforeInput
            },
        )? {
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
    wait_for_paimon_menu(context, config)?;

    context
        .device()
        .press_key(Key::M)
        .map_err(|error| after_input_failure("open_wonderland_map", error))?;

    let map_attempts = capped_attempts(
        config.startup.wonderland_map_star_retries,
        config.startup.enter_wonderland_timeout_ms,
        config.startup.wonderland_map_star_retry_ms,
    );
    let map_interval_ms = config.startup.wonderland_map_star_retry_ms.max(100);
    let map_timeout_ms = (map_attempts as u64).saturating_mul(map_interval_ms).min(
        config
            .startup
            .enter_wonderland_timeout_ms
            .max(map_interval_ms),
    );
    let map_star = wait_template_hit(
        context,
        config,
        TemplateHit {
            template: &config.startup.templates.wonderland_map_star,
            region: config.startup.wonderland_map_star_region,
            threshold: config.startup.template_threshold,
            timeout_ms: map_timeout_ms,
            poll_ms: map_interval_ms,
            certainty: InputCertainty::AfterInputUnknown,
        },
    )?;
    let Some(map_star) = map_star else {
        return Err(UiRoutineFailure::new(
            InputCertainty::AfterInputUnknown,
            "locate_wonderland_map_star",
            "wonderland map star template was not found",
        ));
    };
    context
        .device()
        .click_point(map_star.x, map_star.y)
        .map_err(|error| {
            UiRoutineFailure::new(
                InputCertainty::AfterInputUnknown,
                "click_wonderland_map_star",
                format!("{error:#}"),
            )
        })?;
    *goal_attempted = true;
    wait_template_absent(
        context,
        config,
        TemplateAbsence {
            template: &config.startup.templates.wonderland_map_star,
            region: config.startup.wonderland_map_star_region,
            threshold: config.startup.template_threshold,
            timeout_ms: config.startup.wonderland_transition_timeout_ms,
            stage: "confirm_wonderland_map_star_absent",
            failure_message: "wonderland map star template did not disappear",
        },
    )?;

    let hall_attempts = capped_attempts(
        config.startup.wonderland_hall_retries,
        config.startup.enter_wonderland_timeout_ms,
        config.startup.wonderland_hall_retry_ms,
    );
    let Some(hall_point) = wait_wonderland_hall_text(context, ocr, config, hall_attempts)? else {
        return Err(UiRoutineFailure::new(
            InputCertainty::AfterInputUnknown,
            "locate_wonderland_hall",
            "OCR did not find a 千星奇域/大厅 option",
        ));
    };
    context
        .device()
        .click_point(hall_point.x, hall_point.y)
        .map_err(|error| {
            UiRoutineFailure::new(
                InputCertainty::AfterInputUnknown,
                "select_wonderland_hall",
                format!("{error:#}"),
            )
        })?;

    let Some(confirm_point) = wait_template_hit(
        context,
        config,
        TemplateHit {
            template: &config.startup.templates.wonderland_confirm,
            region: config.startup.wonderland_confirm_region,
            threshold: config.startup.wonderland_confirm_threshold,
            timeout_ms: config.startup.wonderland_transition_timeout_ms,
            poll_ms: config.startup.poll_ms,
            certainty: InputCertainty::AfterInputUnknown,
        },
    )?
    else {
        return Err(UiRoutineFailure::new(
            InputCertainty::AfterInputUnknown,
            "locate_wonderland_confirm",
            "wonderland confirm template was not found",
        ));
    };
    context
        .device()
        .click_point(confirm_point.x, confirm_point.y)
        .map_err(|error| {
            UiRoutineFailure::new(
                InputCertainty::AfterInputUnknown,
                "confirm_enter_wonderland",
                format!("{error:#}"),
            )
        })?;
    confirm_wonderland_transition(context, config)
}

fn wait_for_paimon_menu(
    context: &mut UiRoutineContext<'_>,
    config: &StartupRoutineConfig,
) -> Result<(), UiRoutineFailure> {
    let deadline = Instant::now() + Duration::from_millis(config.residency.timeout_ms());
    let mut streak = 0_u32;
    while Instant::now() < deadline {
        let image = capture_normalized(
            context,
            &config.residency,
            "observe_wonderland_primary",
            InputCertainty::ConfirmedFailure,
        )?;
        if template_visible(
            &image,
            config.startup.main_ui_region,
            &config.startup.templates.paimon_menu,
            config.startup.template_threshold,
            InputCertainty::BeforeInput,
        )? {
            streak = streak.saturating_add(1);
            if streak >= TEMPLATE_STABLE_HITS {
                return Ok(());
            }
        } else {
            streak = 0;
        }
        sleep_ms(config.startup.poll_ms);
    }
    Err(UiRoutineFailure::new(
        InputCertainty::ConfirmedFailure,
        "observe_wonderland_primary",
        "paimon menu template did not become stable before timeout",
    ))
}

fn confirm_wonderland_transition(
    context: &mut UiRoutineContext<'_>,
    config: &StartupRoutineConfig,
) -> Result<(), UiRoutineFailure> {
    let region = config.startup.wonderland_confirm_region;
    let deadline =
        Instant::now() + Duration::from_millis(config.startup.wonderland_transition_timeout_ms);
    while Instant::now() < deadline {
        let image = capture_normalized(
            context,
            &config.residency,
            "confirm_wonderland_absent",
            InputCertainty::AfterInputUnknown,
        )?;
        if !template_visible(
            &image,
            region,
            &config.startup.templates.wonderland_confirm,
            config.startup.wonderland_confirm_threshold,
            InputCertainty::AfterInputUnknown,
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
    let image = capture_normalized(
        context,
        &config.residency,
        "observe_wonderland_transition",
        InputCertainty::AfterInputUnknown,
    )?;
    let mut previous = rect_chat_change_fingerprint(&image, region)
        .map_err(|error| after_input_failure("observe_wonderland_transition", error))?;
    while Instant::now() < deadline {
        sleep_ms(config.startup.poll_ms);
        let image = capture_normalized(
            context,
            &config.residency,
            "confirm_wonderland_stable",
            InputCertainty::AfterInputUnknown,
        )?;
        let current = rect_chat_change_fingerprint(&image, region)
            .map_err(|error| after_input_failure("confirm_wonderland_stable", error))?;
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
    let result = if allow_escape {
        match wait_for_stable_ui_kind(
            context,
            config.residency.state_observation(),
            None,
            timeout_ms,
            "confirm_startup_primary",
            InputCertainty::ConfirmedFailure,
        ) {
            Ok(UiStateKind::Primary) => Ok(UiStateKind::Primary),
            Ok(UiStateKind::Secondary) => {
                if let Err(error) = context.device().press_key(Key::Escape) {
                    return UiResidencyOutcome::Failed(UiRoutineFailure::new(
                        InputCertainty::AfterInputUnknown,
                        "recover_startup_primary",
                        format!("{error:#}"),
                    ));
                }
                wait_for_stable_ui_kind(
                    context,
                    config.residency.state_observation(),
                    Some(UiStateKind::Primary),
                    timeout_ms,
                    "confirm_startup_primary",
                    InputCertainty::AfterInputUnknown,
                )
            }
            Ok(UiStateKind::Unknown) => unreachable!("unknown UI state is never stable"),
            Err(failure) => Err(failure),
        }
    } else {
        wait_for_stable_ui_kind(
            context,
            config.residency.state_observation(),
            Some(UiStateKind::Primary),
            timeout_ms,
            "confirm_startup_primary",
            InputCertainty::AfterInputUnknown,
        )
    };
    match result {
        Ok(UiStateKind::Primary) => UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary),
        Ok(_) => unreachable!("primary wait returned a non-primary state"),
        Err(failure) => UiResidencyOutcome::Failed(failure),
    }
}

fn observe_primary(
    context: &mut UiRoutineContext<'_>,
    config: &StartupRoutineConfig,
) -> UiResidencyOutcome {
    match wait_for_stable_ui_kind(
        context,
        config.residency.state_observation(),
        Some(UiStateKind::Primary),
        config.startup.final_primary_timeout_ms,
        "observe_startup_residency",
        InputCertainty::ConfirmedFailure,
    ) {
        Ok(UiStateKind::Primary) => UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary),
        Ok(_) => unreachable!("primary wait returned a non-primary state"),
        Err(failure) => UiResidencyOutcome::Failed(failure),
    }
}

fn find_enter_game_text(
    ocr: &OcrRuntimeHandle,
    image: &image::DynamicImage,
    config: &StartupRoutineConfig,
    certainty: InputCertainty,
) -> Result<Option<Point>, UiRoutineFailure> {
    let region = config.startup.enter_game_text_region;
    let crop = crop_canvas(image, region).map_err(|error| {
        UiRoutineFailure::new(certainty, "crop_enter_game_text", format!("{error:#}"))
    })?;
    let target = normalize_lock_text(ENTER_GAME_TEXT);
    let lines = ocr
        .recognize_lines(crop, OcrPriority::UiConfirmation)
        .map_err(|error| {
            UiRoutineFailure::new(certainty, "ocr_enter_game_text", format!("{error:#}"))
        })?;
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

fn wait_template_absent(
    context: &mut UiRoutineContext<'_>,
    config: &StartupRoutineConfig,
    wait: TemplateAbsence<'_>,
) -> Result<(), UiRoutineFailure> {
    let deadline = Instant::now() + Duration::from_millis(wait.timeout_ms.max(1));
    while Instant::now() < deadline {
        let image = capture_normalized(
            context,
            &config.residency,
            wait.stage,
            InputCertainty::AfterInputUnknown,
        )?;
        if !template_visible(
            &image,
            wait.region,
            wait.template,
            wait.threshold,
            InputCertainty::AfterInputUnknown,
        )? {
            return Ok(());
        }
        sleep_ms(config.startup.poll_ms);
    }
    Err(UiRoutineFailure::new(
        InputCertainty::AfterInputUnknown,
        wait.stage,
        wait.failure_message,
    ))
}

fn wait_wonderland_hall_text(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &StartupRoutineConfig,
    attempts: u32,
) -> Result<Option<Point>, UiRoutineFailure> {
    for _ in 0..attempts.max(1) {
        let image = capture_normalized(
            context,
            &config.residency,
            "locate_wonderland_hall",
            InputCertainty::AfterInputUnknown,
        )?;
        if let Some(point) = find_wonderland_hall_text(ocr, &image, config)? {
            return Ok(Some(point));
        }
        sleep_ms(config.startup.wonderland_hall_retry_ms.max(100));
    }
    Ok(None)
}

fn find_wonderland_hall_text(
    ocr: &OcrRuntimeHandle,
    image: &image::DynamicImage,
    config: &StartupRoutineConfig,
) -> Result<Option<Point>, UiRoutineFailure> {
    let region = config.startup.wonderland_hall_ocr_region;
    let crop = crop_canvas(image, region)
        .map_err(|error| after_input_failure("crop_wonderland_hall", error))?;
    let lines = ocr
        .recognize_lines(crop, OcrPriority::UiConfirmation)
        .map_err(|error| after_input_failure("ocr_wonderland_hall", error.into()))?;
    Ok(lines.into_iter().find_map(|line| {
        is_wonderland_hall_text(&line.text).then(|| {
            Point::new(
                region.x + line.bbox.center().x,
                region.y + line.bbox.center().y,
            )
        })
    }))
}

fn is_wonderland_hall_text(text: &str) -> bool {
    let normalized = normalize_lock_text(text);
    normalized.contains("千星奇域") || normalized.contains("大厅")
}

fn wait_template_hit(
    context: &mut UiRoutineContext<'_>,
    config: &StartupRoutineConfig,
    wait: TemplateHit<'_>,
) -> Result<Option<Point>, UiRoutineFailure> {
    let deadline = Instant::now() + Duration::from_millis(wait.timeout_ms);
    while Instant::now() < deadline {
        let image = capture_normalized(
            context,
            &config.residency,
            "locate_startup_template",
            wait.certainty,
        )?;
        if let Some(hit) =
            best_template_hit(&image, Some(wait.region), wait.template, wait.threshold).map_err(
                |error| {
                    UiRoutineFailure::new(
                        wait.certainty,
                        "locate_startup_template",
                        format!("{error:#}"),
                    )
                },
            )?
        {
            return Ok(Some(hit.center()));
        }
        sleep_ms(wait.poll_ms.max(1));
    }
    Ok(None)
}

fn template_visible(
    image: &image::DynamicImage,
    region: Rect,
    template: &Path,
    threshold: f32,
    certainty: InputCertainty,
) -> Result<bool, UiRoutineFailure> {
    best_template_hit(image, Some(region), template, threshold)
        .map(|hit| hit.is_some())
        .map_err(|error| {
            UiRoutineFailure::new(certainty, "match_startup_template", format!("{error:#}"))
        })
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
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use crate::config::AppConfig;
    use crate::runtime::ocr::{OcrDevice, OcrLine, OcrRuntime};
    use crate::runtime::ui::{UiDevice, UiRuntime};
    use anyhow::Result;
    use image::{DynamicImage, GenericImage};

    use super::*;

    struct PaimonOnlyDevice {
        frame: DynamicImage,
        keys: Arc<Mutex<Vec<Key>>>,
    }

    struct EmptyOcr;

    impl OcrDevice for EmptyOcr {
        fn recognize_lines(&mut self, _image: &DynamicImage) -> Result<Vec<OcrLine>> {
            Ok(Vec::new())
        }
    }

    impl UiDevice for PaimonOnlyDevice {
        fn capture(&mut self) -> Result<DynamicImage> {
            Ok(self.frame.clone())
        }

        fn press_key(&mut self, key: Key) -> Result<()> {
            self.keys.lock().unwrap().push(key);
            Ok(())
        }

        fn ensure_window(&mut self) -> Result<()> {
            Ok(())
        }

        fn focus(&mut self, _after_activate_ms: u64) -> Result<()> {
            Ok(())
        }
    }

    fn test_startup_config(app: &AppConfig) -> StartupRoutineConfig {
        let startup = &app.startup;
        StartupRoutineConfig::resolve(
            StartupUiConfig {
                launch_game: startup.launch_game,
                enter_game: startup.enter_game,
                exe_path: startup.exe_path.clone(),
                game_args: startup.game_args.clone(),
                launch_wait_ms: startup.launch_wait_ms,
                launch_retries: startup.launch_retries,
                enter_game_timeout_ms: startup.enter_game_timeout_ms,
                enter_wonderland_timeout_ms: startup.enter_wonderland_timeout_ms,
                wonderland_map_star_retries: startup.wonderland_map_star_retries,
                wonderland_map_star_retry_ms: startup.wonderland_map_star_retry_ms,
                wonderland_hall_retries: startup.wonderland_hall_retries,
                wonderland_hall_retry_ms: startup.wonderland_hall_retry_ms,
                wonderland_transition_timeout_ms: startup.wonderland_transition_timeout_ms,
                wonderland_confirm_stable_timeout_ms: startup.wonderland_confirm_stable_timeout_ms,
                final_primary_timeout_ms: startup.final_primary_timeout_ms,
                poll_ms: 1,
                stable_mean_threshold: startup.stable_mean_threshold,
                stable_changed_ratio_threshold: startup.stable_changed_ratio_threshold,
                template_threshold: startup.template_threshold,
                wonderland_confirm_threshold: startup.wonderland_confirm_threshold,
                templates: StartupUiTemplates {
                    wonderland_map_star: startup.templates.wonderland_map_star.clone(),
                    wonderland_confirm: startup.templates.wonderland_confirm.clone(),
                    paimon_menu: startup.templates.paimon_menu.clone(),
                },
                enter_game_text_region: startup.enter_game_text_region.into(),
                wonderland_hall_ocr_region: startup.wonderland_hall_ocr_region.into(),
                wonderland_confirm_region: startup.wonderland_confirm_region.into(),
                main_ui_region: startup.main_ui_region.into(),
                wonderland_map_star_region: startup.wonderland_map_star_region.into(),
            },
            FriendDeliveryRoutineConfig::from_app(app),
            app.window.target_process.clone(),
        )
    }

    #[test]
    fn enter_wonderland_accepts_a_paimon_only_primary_screen() {
        let app = AppConfig::load(Path::new("config.yaml")).unwrap();
        let mut config = test_startup_config(&app);
        config.startup.enter_wonderland_timeout_ms = 1;
        config.startup.wonderland_map_star_retries = 1;
        config.startup.wonderland_map_star_retry_ms = 1;

        let mut frame = DynamicImage::new_rgba8(1920, 1080);
        let paimon = image::open(&config.startup.templates.paimon_menu).unwrap();
        frame
            .copy_from(&paimon, 0, 0)
            .expect("paimon template should fit in the primary region");

        let keys = Arc::new(Mutex::new(Vec::new()));
        let ui_runtime = UiRuntime::start(
            PaimonOnlyDevice {
                frame,
                keys: keys.clone(),
            },
            4,
        )
        .unwrap();
        let ocr_runtime = OcrRuntime::start(EmptyOcr, 1).unwrap();
        config.startup.final_primary_timeout_ms = 1;
        let outcome = ui_runtime
            .handle()
            .submit(EnterWonderlandRoutine {
                request: EnterWonderland,
                ocr: ocr_runtime.handle(),
                config,
            })
            .unwrap()
            .wait()
            .unwrap();

        let EnterWonderlandEffect::Failed(failure) = outcome.effect() else {
            panic!("the fixture intentionally has no wonderland home template");
        };
        assert_eq!(failure.stage(), "locate_wonderland_map_star");
        assert_eq!(failure.certainty(), InputCertainty::AfterInputUnknown);
        assert_eq!(keys.lock().unwrap().as_slice(), &[Key::M]);
        ui_runtime.shutdown().unwrap();
        ocr_runtime.shutdown().unwrap();
    }

    #[test]
    fn wonderland_hall_text_accepts_either_marker() {
        assert!(is_wonderland_hall_text("千星奇域·大厅"));
        assert!(is_wonderland_hall_text("千星奇域"));
        assert!(is_wonderland_hall_text("大厅"));
        assert!(!is_wonderland_hall_text("千星"));
    }

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
