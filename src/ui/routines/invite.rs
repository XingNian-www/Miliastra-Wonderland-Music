use std::path::PathBuf;

use enigo::Key;

use super::friend_delivery::{
    FriendDeliveryRoutineConfig, UiResidencyOutcome, UiResidencyTarget, before_input_failure,
    capture_normalized, current_ui_is_primary, open_friend_conversation, restore_residency,
    send_current_chat_message, sleep_ms,
};
#[cfg(test)]
use crate::config::AppConfig;
use crate::observation::chat::secondary_hall_bubbles;
use crate::runtime::ocr::OcrRuntimeHandle;
use crate::runtime::ui::{
    InputCertainty, UiOperation, UiRoutine, UiRoutineContext, UiRoutineFailure, UiRuntimeHandle,
    UiSubmitError, sealed,
};
use crate::ui::geometry::Rect;
use crate::ui::template::best_template_hit;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExecuteInvite {
    username: String,
    password: Option<String>,
    notification: String,
    residency: UiResidencyTarget,
}

impl ExecuteInvite {
    pub(crate) fn new(
        username: impl Into<String>,
        password: Option<String>,
        notification: impl Into<String>,
        residency: UiResidencyTarget,
    ) -> Self {
        Self {
            username: username.into(),
            password,
            notification: notification.into(),
            residency,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InviteEffect {
    NotAttempted,
    Entered,
    ResultUnknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum InviteNotificationOutcome {
    NotAttempted,
    Sent,
    Failed(UiRoutineFailure),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExecuteInviteOutcome {
    effect: InviteEffect,
    notification: InviteNotificationOutcome,
    failure: Option<UiRoutineFailure>,
    residency: UiResidencyOutcome,
}

impl ExecuteInviteOutcome {
    pub(crate) fn effect(&self) -> InviteEffect {
        self.effect
    }

    pub(crate) fn notification(&self) -> &InviteNotificationOutcome {
        &self.notification
    }

    pub(crate) fn failure(&self) -> Option<&UiRoutineFailure> {
        self.failure.as_ref()
    }

    pub(crate) fn residency(&self) -> &UiResidencyOutcome {
        &self.residency
    }
}

#[derive(Clone)]
pub(crate) struct InviteUi {
    runtime: UiRuntimeHandle,
    ocr: OcrRuntimeHandle,
    config: InviteRoutineConfig,
}

impl InviteUi {
    pub(crate) fn new(
        runtime: UiRuntimeHandle,
        ocr: OcrRuntimeHandle,
        config: InviteRoutineConfig,
    ) -> Self {
        Self {
            runtime,
            ocr,
            config,
        }
    }

    pub(crate) fn submit(
        &self,
        request: ExecuteInvite,
    ) -> std::result::Result<UiOperation<ExecuteInviteOutcome>, UiSubmitError> {
        self.runtime.submit(ExecuteInviteRoutine {
            request,
            ocr: self.ocr.clone(),
            config: self.config.clone(),
        })
    }
}

#[derive(Clone)]
pub(crate) struct InviteRoutineConfig {
    friend: FriendDeliveryRoutineConfig,
    view_star: InviteButton,
    goto_hall: InviteButton,
    enter_hall: InviteButton,
    template_threshold: f32,
    button_timeout_ms: u64,
    completion_timeout_ms: u64,
    poll_ms: u64,
    stable_count: u32,
    click_ms: u64,
    password_step_ms: u64,
    password_digit_ms: u64,
}

pub(crate) struct InviteRoutineConfigSource {
    pub(crate) friend: FriendDeliveryRoutineConfig,
    pub(crate) view_star_template: PathBuf,
    pub(crate) view_star_region: Rect,
    pub(crate) goto_hall_template: PathBuf,
    pub(crate) goto_hall_region: Rect,
    pub(crate) enter_hall_template: PathBuf,
    pub(crate) enter_hall_region: Rect,
    pub(crate) template_threshold: f32,
    pub(crate) button_timeout_ms: u64,
    pub(crate) completion_timeout_ms: u64,
    pub(crate) poll_ms: u64,
    pub(crate) stable_count: u32,
    pub(crate) click_ms: u64,
    pub(crate) password_step_ms: u64,
    pub(crate) password_digit_ms: u64,
}

impl InviteRoutineConfig {
    pub(crate) fn resolve(source: InviteRoutineConfigSource) -> Self {
        Self {
            friend: source.friend,
            view_star: InviteButton {
                path: source.view_star_template,
                region: source.view_star_region,
                stage: "select_wonderland_profile",
            },
            goto_hall: InviteButton {
                path: source.goto_hall_template,
                region: source.goto_hall_region,
                stage: "select_friend_hall",
            },
            enter_hall: InviteButton {
                path: source.enter_hall_template,
                region: source.enter_hall_region,
                stage: "enter_friend_hall",
            },
            template_threshold: source.template_threshold,
            button_timeout_ms: source.button_timeout_ms,
            completion_timeout_ms: source.completion_timeout_ms,
            poll_ms: source.poll_ms.max(10),
            stable_count: source.stable_count.max(1),
            click_ms: source.click_ms,
            password_step_ms: source.password_step_ms,
            password_digit_ms: source.password_digit_ms,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_app(config: &AppConfig) -> Self {
        Self::resolve(InviteRoutineConfigSource {
            friend: FriendDeliveryRoutineConfig::from_app(config),
            view_star_template: config.templates.invite_view_star.clone(),
            view_star_region: config.invite.view_star_region.into(),
            goto_hall_template: config.templates.invite_goto_hall.clone(),
            goto_hall_region: config.invite.goto_hall_region.into(),
            enter_hall_template: config.templates.invite_enter_hall.clone(),
            enter_hall_region: config.invite.enter_hall_region.into(),
            template_threshold: config.templates.marker_threshold,
            button_timeout_ms: config.timing.workflow.default_timeout_ms,
            completion_timeout_ms: config.timing.command.ui_timeout_ms,
            poll_ms: config.timing.workflow.default_poll_ms,
            stable_count: config.resolve_stability_count(config.invite.friend_name_stable_count),
            click_ms: config.timing.input.click_ms,
            password_step_ms: config.timing.invite.step_ms,
            password_digit_ms: config.timing.input.text_ms,
        })
    }
}

#[derive(Clone)]
struct InviteButton {
    path: PathBuf,
    region: Rect,
    stage: &'static str,
}

struct ExecuteInviteRoutine {
    request: ExecuteInvite,
    ocr: OcrRuntimeHandle,
    config: InviteRoutineConfig,
}

impl sealed::UiRoutineSealed for ExecuteInviteRoutine {}

impl UiRoutine for ExecuteInviteRoutine {
    type Output = ExecuteInviteOutcome;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        execute_invite(context, self.request, &self.ocr, &self.config)
    }
}

fn execute_invite(
    context: &mut UiRoutineContext<'_>,
    request: ExecuteInvite,
    ocr: &OcrRuntimeHandle,
    config: &InviteRoutineConfig,
) -> ExecuteInviteOutcome {
    let mut effect = InviteEffect::NotAttempted;
    let mut notification = InviteNotificationOutcome::NotAttempted;
    let mut failure = None;

    if let Err(error) = open_friend_conversation(context, ocr, &config.friend, &request.username) {
        failure = Some(error);
    } else {
        let notification_failed =
            match send_current_chat_message(context, &config.friend, &request.notification) {
                Ok(()) => {
                    notification = InviteNotificationOutcome::Sent;
                    false
                }
                Err(error) => {
                    log::error!("邀请通知发送失败，继续邀请流程: {error}");
                    notification = InviteNotificationOutcome::Failed(error);
                    true
                }
            };

        if notification_failed
            && let Err(error) =
                open_friend_conversation(context, ocr, &config.friend, &request.username)
        {
            failure = Some(error);
        }
        if failure.is_none()
            && let Err(error) =
                execute_invite_navigation(context, config, request.password.as_deref())
        {
            if error.certainty() == InputCertainty::AfterInputUnknown {
                effect = InviteEffect::ResultUnknown;
            }
            failure = Some(error);
        } else if failure.is_none() {
            effect = InviteEffect::Entered;
        }
    }

    let residency = match restore_residency(context, ocr, &config.friend, request.residency) {
        Ok(()) => UiResidencyOutcome::Confirmed(request.residency),
        Err(error) => UiResidencyOutcome::Failed(error),
    };
    ExecuteInviteOutcome {
        effect,
        notification,
        failure,
        residency,
    }
}

fn execute_invite_navigation(
    context: &mut UiRoutineContext<'_>,
    config: &InviteRoutineConfig,
    password: Option<&str>,
) -> std::result::Result<(), UiRoutineFailure> {
    click_current_friend_avatar(context, config)?;

    click_invite_button(
        context,
        config,
        &config.view_star,
        InputCertainty::BeforeInput,
    )?;
    click_invite_button(
        context,
        config,
        &config.goto_hall,
        InputCertainty::BeforeInput,
    )?;
    click_invite_button(
        context,
        config,
        &config.enter_hall,
        InputCertainty::AfterInputUnknown,
    )?;

    if let Some(password) = password {
        sleep_ms(config.password_step_ms);
        for digit in password.chars() {
            context
                .device()
                .press_key(Key::Unicode(digit))
                .map_err(|error| {
                    UiRoutineFailure::new(
                        InputCertainty::AfterInputUnknown,
                        "enter_hall_password",
                        format!("{error:#}"),
                    )
                })?;
            sleep_ms(config.password_digit_ms);
        }
    }
    confirm_entered_hall(context, config)
}

fn click_current_friend_avatar(
    context: &mut UiRoutineContext<'_>,
    config: &InviteRoutineConfig,
) -> std::result::Result<(), UiRoutineFailure> {
    let total_attempts = attempts(
        config.button_timeout_ms,
        config.poll_ms,
        config.stable_count,
    );
    let mut stable_point = None;
    let mut streak = 0_u32;
    for attempt in 0..total_attempts {
        let image = capture_normalized(
            context,
            &config.friend,
            "select_invite_avatar",
            InputCertainty::BeforeInput,
        )?;
        let point = secondary_hall_bubbles(&image)
            .map_err(|error| before_input_failure("select_invite_avatar", error))?
            .into_iter()
            .max_by_key(|bubble| bubble.avatar_rect().y)
            .map(|bubble| bubble.avatar_rect().center());
        match point {
            Some(point)
                if stable_point.is_some_and(|previous: (i32, i32)| {
                    previous.0.abs_diff(point.x) <= 4 && previous.1.abs_diff(point.y) <= 4
                }) =>
            {
                streak = streak.saturating_add(1);
                stable_point = Some((point.x, point.y));
            }
            Some(point) => {
                stable_point = Some((point.x, point.y));
                streak = 1;
            }
            None => {
                stable_point = None;
                streak = 0;
            }
        }
        if streak >= config.stable_count {
            let (x, y) = stable_point.expect("stable invite avatar point exists");
            context
                .device()
                .click_point(x, y)
                .map_err(|error| before_input_failure("select_invite_avatar", error))?;
            log::info!(
                "邀请: 当前好友会话的左侧头像稳定确认，点击 samples={} x={} y={}",
                config.stable_count,
                x,
                y
            );
            sleep_ms(config.click_ms);
            return Ok(());
        }
        if attempt + 1 < total_attempts {
            sleep_ms(config.poll_ms);
        }
    }
    Err(UiRoutineFailure::new(
        InputCertainty::ConfirmedFailure,
        "select_invite_avatar",
        "a stable incoming avatar was not found in the confirmed friend conversation",
    ))
}

fn click_invite_button(
    context: &mut UiRoutineContext<'_>,
    config: &InviteRoutineConfig,
    button: &InviteButton,
    click_certainty: InputCertainty,
) -> std::result::Result<(), UiRoutineFailure> {
    let total_attempts = attempts(
        config.button_timeout_ms,
        config.poll_ms,
        config.stable_count,
    );
    let mut stable_point = None;
    let mut streak = 0_u32;
    for attempt in 0..total_attempts {
        let image = capture_normalized(context, &config.friend, button.stage, click_certainty)?;
        let hit = best_template_hit(
            &image,
            Some(button.region),
            &button.path,
            config.template_threshold,
        )
        .map_err(|error| before_input_failure(button.stage, error))?;
        let point = hit.map(|hit| hit.center());
        match point {
            Some(point)
                if stable_point.is_some_and(|previous: (i32, i32)| {
                    previous.0.abs_diff(point.x) <= 4 && previous.1.abs_diff(point.y) <= 4
                }) =>
            {
                streak = streak.saturating_add(1);
                stable_point = Some((point.x, point.y));
            }
            Some(point) => {
                stable_point = Some((point.x, point.y));
                streak = 1;
            }
            None => {
                stable_point = None;
                streak = 0;
            }
        }
        if streak >= config.stable_count {
            let (x, y) = stable_point.expect("stable invite template point exists");
            context.device().click_point(x, y).map_err(|error| {
                UiRoutineFailure::new(click_certainty, button.stage, format!("{error:#}"))
            })?;
            sleep_ms(config.click_ms);
            return Ok(());
        }
        if attempt + 1 < total_attempts {
            sleep_ms(config.poll_ms);
        }
    }
    Err(UiRoutineFailure::new(
        InputCertainty::ConfirmedFailure,
        button.stage,
        format!("template was not found: {}", button.path.display()),
    ))
}

fn confirm_entered_hall(
    context: &mut UiRoutineContext<'_>,
    config: &InviteRoutineConfig,
) -> std::result::Result<(), UiRoutineFailure> {
    let attempts = attempts(
        config.completion_timeout_ms,
        config.poll_ms,
        config.stable_count,
    );
    let mut streak = 0_u32;
    for attempt in 0..attempts {
        if current_ui_is_primary(context, &config.friend, "confirm_entered_hall")? {
            streak = streak.saturating_add(1);
            if streak >= config.stable_count {
                return Ok(());
            }
        } else {
            streak = 0;
        }
        if attempt + 1 < attempts {
            sleep_ms(config.poll_ms);
        }
    }
    Err(UiRoutineFailure::new(
        InputCertainty::AfterInputUnknown,
        "confirm_entered_hall",
        "the hall entry result did not become a stable primary UI before timeout",
    ))
}

fn attempts(timeout_ms: u64, poll_ms: u64, minimum: u32) -> u32 {
    (timeout_ms / poll_ms.max(1))
        .max(minimum as u64)
        .min(u32::MAX as u64) as u32
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use anyhow::{Result, bail};
    use enigo::Key;
    use image::{DynamicImage, GenericImage, Rgba};

    use super::*;
    use crate::config::{AppConfig, RectConfig};
    use crate::observation::chat::SECONDARY_TITLE_RECT;
    use crate::runtime::ocr::{OcrDevice, OcrLine, OcrRuntime};
    use crate::runtime::ui::{UiDevice, UiRuntime};
    use crate::ui::geometry::Rect;
    use crate::ui::state::{TemplateUiStateClassifier, UiTemplateArgs};

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    enum InvitePhase {
        Hall,
        Friend,
        ViewStar,
        GotoHall,
        EnterHall,
        Primary,
    }

    struct InviteTestState {
        phase: InvitePhase,
        pasted: Vec<String>,
        clicks: Vec<&'static str>,
    }

    struct InviteDevice {
        state: Arc<Mutex<InviteTestState>>,
        frames: HashMap<InvitePhase, DynamicImage>,
        friend_row: (i32, i32),
        friend_avatar: (i32, i32),
        view_star: (i32, i32),
        goto_hall: (i32, i32),
        enter_hall: (i32, i32),
    }

    impl UiDevice for InviteDevice {
        fn capture(&mut self) -> Result<DynamicImage> {
            Ok(self.frames[&self.state.lock().unwrap().phase].clone())
        }

        fn ensure_ready(&mut self, _after_activate_ms: u64) -> Result<()> {
            Ok(())
        }

        fn ensure_foreground(&mut self) -> Result<()> {
            Ok(())
        }

        fn click_point(&mut self, x: i32, y: i32) -> Result<()> {
            let point = (x, y);
            let mut state = self.state.lock().unwrap();
            if near(point, self.friend_row) {
                state.phase = InvitePhase::Friend;
                state.clicks.push("friend");
            } else if near(point, self.friend_avatar) {
                state.phase = InvitePhase::ViewStar;
                state.clicks.push("friend_avatar");
            } else if point == self.view_star {
                state.phase = InvitePhase::GotoHall;
                state.clicks.push("view_star");
            } else if point == self.goto_hall {
                state.phase = InvitePhase::EnterHall;
                state.clicks.push("goto_hall");
            } else if point == self.enter_hall {
                state.phase = InvitePhase::Primary;
                state.clicks.push("enter_hall");
            }
            Ok(())
        }

        fn paste_text(&mut self, text: &str, _clipboard_hold_ms: u64) -> Result<()> {
            self.state.lock().unwrap().pasted.push(text.to_string());
            Ok(())
        }

        fn input_text(&mut self, _text: &str, _input_settle_ms: u64) -> Result<()> {
            bail!("paste should succeed")
        }

        fn press_key(&mut self, _key: Key) -> Result<()> {
            Ok(())
        }

        fn scroll_point(&mut self, _x: i32, _y: i32, _length: i32) -> Result<()> {
            bail!("the friend is visible on the first page")
        }
    }

    struct InviteOcrDevice {
        state: Arc<Mutex<InviteTestState>>,
        friend_list_size: (u32, u32),
        title_size: (u32, u32),
    }

    impl OcrDevice for InviteOcrDevice {
        fn recognize_lines(&mut self, image: &DynamicImage) -> Result<Vec<OcrLine>> {
            let size = (image.width(), image.height());
            if size == self.friend_list_size {
                return Ok(vec![line("甲", 5, 70, 80, 30)]);
            }
            if size == self.title_size {
                let title = match self.state.lock().unwrap().phase {
                    InvitePhase::Hall => "当前大厅",
                    _ => "甲",
                };
                return Ok(vec![line(title, 0, 0, image.width(), image.height())]);
            }
            Ok(Vec::new())
        }
    }

    #[test]
    fn invite_notification_and_navigation_run_in_one_ui_operation() {
        let mut config = AppConfig::load(Path::new("config.yaml")).unwrap();
        config.timing.input.after_activate_ms = 0;
        config.timing.input.open_chat_ms = 0;
        config.timing.input.click_ms = 0;
        config.timing.input.text_ms = 0;
        config.timing.input.send_ms = 0;
        config.timing.invite.step_ms = 0;
        config.timing.workflow.default_timeout_ms = 20;
        config.timing.workflow.default_poll_ms = 1;
        config.timing.command.ui_timeout_ms = 20;
        let state = Arc::new(Mutex::new(InviteTestState {
            phase: InvitePhase::Hall,
            pasted: Vec::new(),
            clicks: Vec::new(),
        }));
        let (frames, friend_avatar, view_star, goto_hall, enter_hall) = invite_frames(&config);
        let friend_list = config.invite.friend_list_region;
        let title = SECONDARY_TITLE_RECT;
        let ui_runtime = UiRuntime::start_with_state_classifier(
            InviteDevice {
                state: state.clone(),
                frames,
                friend_row: (friend_list.x + 45, friend_list.y + 85),
                friend_avatar,
                view_star,
                goto_hall,
                enter_hall,
            },
            2,
            TemplateUiStateClassifier::new(
                UiTemplateArgs::default().resolve(&config.templates, &config.ocr),
                config.screen.clone(),
            ),
            config.resolve_stability_count(0),
        )
        .unwrap();
        let ocr_runtime = OcrRuntime::start(
            InviteOcrDevice {
                state: state.clone(),
                friend_list_size: (friend_list.width, friend_list.height),
                title_size: (title.width, title.height),
            },
            4,
        )
        .unwrap();
        let invite_ui = InviteUi::new(
            ui_runtime.handle(),
            ocr_runtime.handle(),
            InviteRoutineConfig::from_app(&config),
        );

        let outcome = invite_ui
            .submit(ExecuteInvite::new(
                "甲",
                None,
                "已同意加入大厅",
                UiResidencyTarget::Primary,
            ))
            .unwrap()
            .wait()
            .unwrap();

        assert_eq!(outcome.effect(), InviteEffect::Entered);
        assert_eq!(outcome.notification(), &InviteNotificationOutcome::Sent);
        assert!(matches!(
            outcome.residency(),
            UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary)
        ));
        let state = state.lock().unwrap();
        assert_eq!(state.pasted, ["已同意加入大厅"]);
        assert_eq!(
            state.clicks,
            [
                "friend",
                "friend_avatar",
                "view_star",
                "goto_hall",
                "enter_hall",
            ]
        );

        ui_runtime.shutdown().unwrap();
        ocr_runtime.shutdown().unwrap();
    }

    type InviteFrameSet = (
        HashMap<InvitePhase, DynamicImage>,
        (i32, i32),
        (i32, i32),
        (i32, i32),
        (i32, i32),
    );

    fn invite_frames(config: &AppConfig) -> InviteFrameSet {
        let (secondary, friend_avatar) = secondary_frame(config);
        let mut view_frame = secondary.clone();
        let view_star = place_template(
            &mut view_frame,
            &config.templates.invite_view_star,
            config.invite.view_star_region,
        );
        let mut goto_frame = secondary.clone();
        let goto_hall = place_template(
            &mut goto_frame,
            &config.templates.invite_goto_hall,
            config.invite.goto_hall_region,
        );
        let mut enter_frame = secondary.clone();
        let enter_hall = place_template(
            &mut enter_frame,
            &config.templates.invite_enter_hall,
            config.invite.enter_hall_region,
        );
        let mut primary =
            DynamicImage::new_rgba8(config.screen.expected_width, config.screen.expected_height);
        place_template(
            &mut primary,
            &config.templates.friend,
            config.screen.friend_rect,
        );
        let frames = HashMap::from([
            (InvitePhase::Hall, secondary.clone()),
            (InvitePhase::Friend, secondary),
            (InvitePhase::ViewStar, view_frame),
            (InvitePhase::GotoHall, goto_frame),
            (InvitePhase::EnterHall, enter_frame),
            (InvitePhase::Primary, primary),
        ]);
        (frames, friend_avatar, view_star, goto_hall, enter_hall)
    }

    fn secondary_frame(config: &AppConfig) -> (DynamicImage, (i32, i32)) {
        let mut frame =
            DynamicImage::new_rgba8(config.screen.expected_width, config.screen.expected_height);
        place_template(
            &mut frame,
            &config.templates.secondary_back,
            config.screen.secondary_back_rect,
        );
        let avatar = Rect::new(302, 204, 88, 88);
        let center = avatar.center();
        draw_avatar(&mut frame, avatar);
        fill_rect(
            &mut frame,
            Rect::new(410, 240, 190, 60),
            Rgba([62, 71, 89, 255]),
        );
        (frame, (center.x, center.y))
    }

    fn draw_avatar(frame: &mut DynamicImage, rect: Rect) {
        let center = rect.center();
        let radius_squared = (rect.width as i32 / 2 - 4).pow(2);
        for y in rect.y..rect.bottom() {
            for x in rect.x..rect.right() {
                let dx = x - center.x;
                let dy = y - center.y;
                if dx * dx + dy * dy <= radius_squared {
                    frame.put_pixel(x as u32, y as u32, Rgba([220, 220, 220, 255]));
                }
            }
        }
    }

    fn fill_rect(frame: &mut DynamicImage, rect: Rect, color: Rgba<u8>) {
        for y in rect.y..rect.bottom() {
            for x in rect.x..rect.right() {
                frame.put_pixel(x as u32, y as u32, color);
            }
        }
    }

    fn place_template(frame: &mut DynamicImage, path: &Path, region: RectConfig) -> (i32, i32) {
        let template = image::open(path).unwrap();
        frame
            .copy_from(&template, region.x as u32, region.y as u32)
            .unwrap();
        (
            region.x + template.width() as i32 / 2,
            region.y + template.height() as i32 / 2,
        )
    }

    fn line(text: &str, x: i32, y: i32, width: u32, height: u32) -> OcrLine {
        OcrLine {
            text: text.to_string(),
            confidence: 1.0,
            bbox: Rect::new(x, y, width, height),
        }
    }

    fn near(left: (i32, i32), right: (i32, i32)) -> bool {
        left.0.abs_diff(right.0) <= 10 && left.1.abs_diff(right.1) <= 10
    }
}
