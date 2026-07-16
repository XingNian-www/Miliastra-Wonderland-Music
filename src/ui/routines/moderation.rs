use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use enigo::Key;

use super::friend_delivery::{
    FriendDeliveryRoutineConfig, UiResidencyOutcome, UiResidencyTarget, before_input_failure,
    capture_normalized, sleep_ms,
};
use crate::config::AppConfig;
use crate::runtime::ui::{
    InputCertainty, UiOperation, UiRoutine, UiRoutineContext, UiRoutineFailure, UiRuntimeHandle,
    UiSubmitError, sealed,
};
use crate::ui::geometry::{Point, Rect};
use crate::ui::state::{ResolvedUiTemplateArgs, UiTemplateArgs, detect_ui_state};
use crate::ui::template::best_template_hit;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ModerationUiAction {
    Blacklist,
    BlockChat,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExecuteModeration {
    action: ModerationUiAction,
    uid: String,
}

impl ExecuteModeration {
    pub(crate) fn new(action: ModerationUiAction, uid: impl Into<String>) -> Self {
        Self {
            action,
            uid: uid.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ModerationEffect {
    Applied,
    Failed(UiRoutineFailure),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExecuteModerationOutcome {
    effect: ModerationEffect,
    residency: UiResidencyOutcome,
}

impl ExecuteModerationOutcome {
    pub(crate) fn effect(&self) -> &ModerationEffect {
        &self.effect
    }

    pub(crate) fn residency(&self) -> &UiResidencyOutcome {
        &self.residency
    }
}

#[derive(Clone)]
pub(crate) struct ModerationUi {
    runtime: UiRuntimeHandle,
    config: ModerationRoutineConfig,
}

impl ModerationUi {
    pub(crate) fn new(runtime: UiRuntimeHandle, config: &AppConfig) -> Self {
        Self {
            runtime,
            config: ModerationRoutineConfig::from_app(config),
        }
    }

    pub(crate) fn submit(
        &self,
        request: ExecuteModeration,
    ) -> Result<UiOperation<ExecuteModerationOutcome>, UiSubmitError> {
        self.runtime.submit(ExecuteModerationRoutine {
            request,
            config: self.config.clone(),
        })
    }
}

#[derive(Clone)]
struct ModerationRoutineConfig {
    residency: FriendDeliveryRoutineConfig,
    templates: ResolvedUiTemplateArgs,
    friend_panel_template: PathBuf,
    search_panel_template: PathBuf,
    more_settings_template: PathBuf,
    blacklist_template: PathBuf,
    block_chat_template: PathBuf,
    confirm_template: PathBuf,
    friend_panel_region: Rect,
    search_panel_region: Rect,
    more_settings_region: Rect,
    blacklist_region: Rect,
    block_chat_region: Rect,
    confirm_region: Rect,
    search_input: Point,
    search_button: Point,
    marker_threshold: f32,
    ui_timeout_ms: u64,
    search_timeout_ms: u64,
    confirm_wait_ms: u64,
    step_ms: u64,
    text_ms: u64,
    return_retry_ms: u64,
}

impl ModerationRoutineConfig {
    fn from_app(config: &AppConfig) -> Self {
        Self {
            residency: FriendDeliveryRoutineConfig::from_app(config),
            templates: UiTemplateArgs::default().resolve(config),
            friend_panel_template: config.templates.friend_panel.clone(),
            search_panel_template: config.templates.friend_search_panel.clone(),
            more_settings_template: config.templates.friend_more_settings.clone(),
            blacklist_template: config.templates.friend_blacklist.clone(),
            block_chat_template: config.templates.friend_block_chat.clone(),
            confirm_template: config.templates.friend_confirm.clone(),
            friend_panel_region: config.moderation.friend_panel_region.into(),
            search_panel_region: config.moderation.search_panel_region.into(),
            more_settings_region: config.moderation.more_settings_region.into(),
            blacklist_region: config.moderation.blacklist_region.into(),
            block_chat_region: config.moderation.block_chat_region.into(),
            confirm_region: config.moderation.confirm_region.into(),
            search_input: Point::new(
                config.moderation.search_input_point.x,
                config.moderation.search_input_point.y,
            ),
            search_button: Point::new(
                config.moderation.search_button_point.x,
                config.moderation.search_button_point.y,
            ),
            marker_threshold: config.templates.marker_threshold,
            ui_timeout_ms: config.timing.command.ui_timeout_ms,
            search_timeout_ms: config.timing.moderation.search_result_timeout_ms,
            confirm_wait_ms: config.timing.moderation.confirm_wait_ms,
            step_ms: config.timing.invite.step_ms,
            text_ms: config.timing.input.text_ms,
            return_retry_ms: config.timing.command.return_retry_ms,
        }
    }
}

struct ExecuteModerationRoutine {
    request: ExecuteModeration,
    config: ModerationRoutineConfig,
}

impl sealed::UiRoutineSealed for ExecuteModerationRoutine {}

impl UiRoutine for ExecuteModerationRoutine {
    type Output = ExecuteModerationOutcome;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        let effect = match execute_moderation(context, &self.request, &self.config) {
            Ok(()) => ModerationEffect::Applied,
            Err(failure) => ModerationEffect::Failed(failure),
        };
        let residency = recover_primary(context, &self.config);
        ExecuteModerationOutcome { effect, residency }
    }
}

fn execute_moderation(
    context: &mut UiRoutineContext<'_>,
    request: &ExecuteModeration,
    config: &ModerationRoutineConfig,
) -> Result<(), UiRoutineFailure> {
    normalize_primary(context, config)?;
    context
        .device()
        .press_key(Key::Unicode('o'))
        .map_err(|error| before_input_failure("open_friend_panel", error))?;
    wait_template(
        context,
        config,
        &config.friend_panel_template,
        config.friend_panel_region,
        config.ui_timeout_ms,
        false,
        "confirm_friend_panel",
    )?;

    context
        .device()
        .press_key(Key::Unicode('e'))
        .map_err(|error| before_input_failure("open_friend_search", error))?;
    sleep_ms(config.step_ms);
    context
        .device()
        .press_key(Key::Unicode('e'))
        .map_err(|error| before_input_failure("open_friend_search", error))?;
    wait_template(
        context,
        config,
        &config.search_panel_template,
        config.search_panel_region,
        config.ui_timeout_ms,
        false,
        "confirm_friend_search",
    )?;

    context
        .device()
        .click_point(config.search_input.x, config.search_input.y)
        .map_err(|error| before_input_failure("focus_uid_input", error))?;
    sleep_ms(config.residency.click_ms);
    context
        .device()
        .paste_text(&request.uid, config.text_ms)
        .map_err(|error| before_input_failure("input_uid", error))?;
    context
        .device()
        .click_point(config.search_button.x, config.search_button.y)
        .map_err(|error| before_input_failure("submit_uid_search", error))?;
    wait_template(
        context,
        config,
        &config.more_settings_template,
        config.more_settings_region,
        config.search_timeout_ms,
        true,
        "open_more_settings",
    )?;

    let (action_template, action_region) = match request.action {
        ModerationUiAction::Blacklist => (&config.blacklist_template, config.blacklist_region),
        ModerationUiAction::BlockChat => (&config.block_chat_template, config.block_chat_region),
    };
    wait_template(
        context,
        config,
        action_template,
        action_region,
        config.ui_timeout_ms,
        true,
        "select_moderation_action",
    )?;
    wait_template(
        context,
        config,
        &config.confirm_template,
        config.confirm_region,
        config.ui_timeout_ms,
        true,
        "confirm_moderation_action",
    )?;
    confirm_template_absent(context, config)
}

fn normalize_primary(
    context: &mut UiRoutineContext<'_>,
    config: &ModerationRoutineConfig,
) -> Result<(), UiRoutineFailure> {
    context
        .device()
        .ensure_ready(config.residency.after_activate_ms)
        .map_err(|error| before_input_failure("prepare_moderation", error))?;
    let image = capture_normalized(context, &config.residency, "observe_moderation_start")?;
    let state = detect_ui_state(&image, &config.templates, &config.residency.screen)
        .map_err(|error| before_input_failure("classify_moderation_start", error))?;
    if state.is_primary() {
        Ok(())
    } else {
        Err(UiRoutineFailure::new(
            InputCertainty::BeforeInput,
            "normalize_moderation_start",
            "moderation requires a stable primary UI",
        ))
    }
}

#[allow(clippy::too_many_arguments)]
fn wait_template(
    context: &mut UiRoutineContext<'_>,
    config: &ModerationRoutineConfig,
    template: &Path,
    region: Rect,
    timeout_ms: u64,
    click: bool,
    stage: &'static str,
) -> Result<(), UiRoutineFailure> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let image = capture_normalized(context, &config.residency, stage)?;
        if let Some(hit) =
            best_template_hit(&image, Some(region), template, config.marker_threshold)
                .map_err(|error| before_input_failure(stage, error))?
        {
            if click {
                let point = hit.center();
                context
                    .device()
                    .click_point(point.x, point.y)
                    .map_err(|error| before_input_failure(stage, error))?;
                sleep_ms(config.residency.click_ms);
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(UiRoutineFailure::new(
                InputCertainty::ConfirmedFailure,
                stage,
                "required moderation template was not found before timeout",
            ));
        }
        sleep_ms(config.residency.poll_ms);
    }
}

fn confirm_template_absent(
    context: &mut UiRoutineContext<'_>,
    config: &ModerationRoutineConfig,
) -> Result<(), UiRoutineFailure> {
    let deadline = Instant::now() + Duration::from_millis(config.confirm_wait_ms);
    let mut absent_streak = 0_u32;
    while Instant::now() < deadline {
        let image = capture_normalized(context, &config.residency, "confirm_action_applied")?;
        let present = best_template_hit(
            &image,
            Some(config.confirm_region),
            &config.confirm_template,
            config.marker_threshold,
        )
        .map_err(|error| {
            UiRoutineFailure::new(
                InputCertainty::AfterInputUnknown,
                "confirm_action_applied",
                format!("{error:#}"),
            )
        })?
        .is_some();
        if present {
            absent_streak = 0;
        } else {
            absent_streak = absent_streak.saturating_add(1);
            if absent_streak >= config.residency.stable_count {
                return Ok(());
            }
        }
        sleep_ms(config.residency.poll_ms);
    }
    Err(UiRoutineFailure::new(
        InputCertainty::AfterInputUnknown,
        "confirm_action_applied",
        "confirmation template did not stably disappear",
    ))
}

fn recover_primary(
    context: &mut UiRoutineContext<'_>,
    config: &ModerationRoutineConfig,
) -> UiResidencyOutcome {
    for _ in 0..6 {
        let image = match capture_normalized(context, &config.residency, "recover_moderation") {
            Ok(image) => image,
            Err(failure) => return UiResidencyOutcome::Failed(failure),
        };
        let state = match detect_ui_state(&image, &config.templates, &config.residency.screen) {
            Ok(state) => state,
            Err(error) => {
                return UiResidencyOutcome::Failed(before_input_failure(
                    "recover_moderation",
                    error,
                ));
            }
        };
        if state.is_primary() {
            return UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary);
        }
        if let Err(error) = context.device().press_key(Key::Escape) {
            return UiResidencyOutcome::Failed(UiRoutineFailure::new(
                InputCertainty::AfterInputUnknown,
                "recover_moderation",
                format!("{error:#}"),
            ));
        }
        sleep_ms(config.return_retry_ms);
    }
    UiResidencyOutcome::Failed(UiRoutineFailure::new(
        InputCertainty::ConfirmedFailure,
        "recover_moderation",
        "primary UI was not reached after bounded recovery",
    ))
}
