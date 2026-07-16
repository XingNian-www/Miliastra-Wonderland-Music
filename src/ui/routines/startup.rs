use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use enigo::Key;
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_VALUE_TYPE, RRF_RT_REG_SZ, RegGetValueW,
};
use windows::core::w;

use super::friend_delivery::{
    FriendDeliveryRoutineConfig, UiResidencyOutcome, UiResidencyTarget, before_input_failure,
    capture_normalized, restore_residency, sleep_ms,
};
use crate::config::{AppConfig, StartupConfig};
use crate::runtime::ocr::{OcrPriority, OcrRuntimeHandle};
use crate::runtime::ui::{
    InputCertainty, UiOperation, UiRoutine, UiRoutineContext, UiRoutineFailure, UiRuntimeHandle,
    UiSubmitError, sealed,
};
use crate::text::normalize_comparison_text as normalize_lock_text;
use crate::ui::change_detection::{change_stats, rect_chat_change_fingerprint};
use crate::ui::geometry::{Point, Rect, crop_canvas};
use crate::ui::state::{ResolvedUiTemplateArgs, UiTemplateArgs, detect_ui_state};
use crate::ui::template::best_template_hit;

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
    pub(crate) fn new(runtime: UiRuntimeHandle, ocr: OcrRuntimeHandle, config: &AppConfig) -> Self {
        Self {
            runtime,
            ocr,
            config: StartupRoutineConfig::from_app(config),
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
struct StartupRoutineConfig {
    startup: StartupConfig,
    residency: FriendDeliveryRoutineConfig,
    templates: ResolvedUiTemplateArgs,
    target_process: String,
}

impl StartupRoutineConfig {
    fn from_app(config: &AppConfig) -> Self {
        Self {
            startup: config.startup.clone(),
            residency: FriendDeliveryRoutineConfig::from_app(config),
            templates: UiTemplateArgs::default().resolve(config),
            target_process: config.window.target_process.clone(),
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
            config.startup.main_ui_region.into(),
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
    let executable = resolve_game_path(&config.startup, &config.target_process)
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
            config.startup.wonderland_close_region.into(),
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
            config.startup.wonderland_enter_button_region.into(),
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
    let region: Rect = config.startup.wonderland_enter_button_region.into();
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
    let region: Rect = config.startup.enter_game_text_region.into();
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

fn resolve_game_path(startup: &StartupConfig, target_process: &str) -> anyhow::Result<PathBuf> {
    if !startup.exe_path.as_os_str().is_empty() {
        if startup.exe_path.is_dir() {
            return resolve_game_path_from_dir(&startup.exe_path, target_process);
        }
        return Ok(startup.exe_path.clone());
    }
    registry_game_path().ok_or_else(|| {
        anyhow::anyhow!("startup.exe_path is empty and no launcher registry path was found")
    })
}

fn resolve_game_path_from_dir(dir: &Path, target_process: &str) -> anyhow::Result<PathBuf> {
    for candidate in startup_exe_candidates(target_process) {
        let path = dir.join(candidate);
        if path.exists() {
            return Ok(path);
        }
    }
    anyhow::bail!("no target game executable exists in {}", dir.display())
}

fn startup_exe_candidates(target_process: &str) -> Vec<String> {
    let mut candidates = target_process
        .split(|ch: char| ch == ',' || ch == ';' || ch == '|' || ch.is_whitespace())
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| {
            if item.to_ascii_lowercase().ends_with(".exe") {
                item.to_string()
            } else {
                format!("{item}.exe")
            }
        })
        .collect::<Vec<_>>();
    for fallback in ["YuanShen.exe", "GenshinImpact.exe"] {
        if !candidates
            .iter()
            .any(|item| item.eq_ignore_ascii_case(fallback))
        {
            candidates.push(fallback.to_string());
        }
    }
    candidates
}

fn registry_game_path() -> Option<PathBuf> {
    for (key, exe) in [
        (w!("Software\\miHoYo\\HYP\\1_1\\hk4e_cn"), "YuanShen.exe"),
        (
            w!("Software\\Cognosphere\\HYP\\1_0\\hk4e_global"),
            "GenshinImpact.exe",
        ),
    ] {
        let Some(dir) = registry_string(key, w!("GameInstallPath")) else {
            continue;
        };
        let path = Path::new(dir.trim()).join(exe);
        if path.exists() {
            return Some(path);
        }
    }
    None
}

fn registry_string(key: windows::core::PCWSTR, value: windows::core::PCWSTR) -> Option<String> {
    let mut value_type = REG_VALUE_TYPE::default();
    let mut byte_len = 0_u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key,
            value,
            RRF_RT_REG_SZ,
            Some(&mut value_type),
            None,
            Some(&mut byte_len),
        )
    };
    if status.0 != 0 || byte_len == 0 {
        return None;
    }
    let mut buffer = vec![0_u16; (byte_len as usize).div_ceil(2)];
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key,
            value,
            RRF_RT_REG_SZ,
            Some(&mut value_type),
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut byte_len),
        )
    };
    if status.0 != 0 {
        return None;
    }
    let len = buffer
        .iter()
        .position(|ch| *ch == 0)
        .unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..len]))
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

    #[test]
    fn startup_exe_candidates_adds_suffix_and_fallbacks() {
        let candidates = startup_exe_candidates("yuanshen.exe, GenshinImpact");
        assert_eq!(candidates[0], "yuanshen.exe");
        assert_eq!(candidates[1], "GenshinImpact.exe");
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case("YuanShen.exe"))
        );
    }
}
