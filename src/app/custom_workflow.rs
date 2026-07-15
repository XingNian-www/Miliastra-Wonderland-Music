use std::path::Path;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::time::{Duration, Instant};

use super::command::{CustomWorkflowCommand, ParsedCommand, UserCommand};
use super::frame_source::Canvas;
use super::ocr_runtime::OcrPriority;
use super::ui_locator::UiLocator;
use super::workflow_actions::{self, HitAction, PixelStability, TemplateMode};
use super::{AutomationApp, ChatDecisionScope, FrameArgs, UiResidency};
use crate::config::{self, AppConfig, PointConfig, RectConfig};
use crate::features::custom_workflow::{
    CustomWorkflowExecutionPort, CustomWorkflowInvocation, CustomWorkflowMatch,
    CustomWorkflowService, FreshMessageOutcome, WorkflowConfirmation, WorkflowDefaults,
    WorkflowOperation, WorkflowPixelStability, WorkflowPoint, WorkflowRect, WorkflowResidency,
};
use crate::features::invite::{InviteDecision, InviteExecutionPort, InviteRequest, InviteStart};
use anyhow::{Result, anyhow};

pub(super) fn service_from_config(config: &AppConfig) -> CustomWorkflowService {
    CustomWorkflowService::new(
        config.custom_workflows.clone(),
        WorkflowDefaults {
            default_timeout_ms: config.timing.workflow.default_timeout_ms,
            default_poll_ms: config.timing.workflow.default_poll_ms,
            default_step_wait_ms: config.timing.workflow.default_step_wait_ms,
            decision_timeout_ms: config.timing.decision.timeout_ms,
            decision_poll_ms: config.timing.decision.poll_ms,
            after_activate_ms: config.timing.input.after_activate_ms,
            clipboard_hold_ms: config.timing.input.text_ms,
            stability_mean_threshold: config.ocr.change_mean_threshold,
            stability_changed_ratio_threshold: config.ocr.change_pixel_threshold,
        },
    )
}

pub(super) fn into_parsed_command(matched: CustomWorkflowMatch) -> ParsedCommand {
    ParsedCommand {
        matched: matched.matched,
        raw: matched.raw,
        user_command: matched.user_command,
        message_type: matched.message_type,
        username: matched.username,
        command: UserCommand::CustomWorkflow(matched.command),
    }
}

enum ChatDecisionWait<T> {
    Found(T),
    Timeout,
    Stopped,
}

impl AutomationApp {
    pub(super) fn execute_custom_workflow(
        &mut self,
        command: &CustomWorkflowCommand,
        parsed: &ParsedCommand,
    ) -> Result<()> {
        let invocation = CustomWorkflowInvocation {
            command: command.clone(),
            username: parsed.username.clone(),
            message_type: parsed.message_type.clone(),
            user_command: parsed.user_command.clone(),
        };
        let service = self.custom_workflow.clone();
        service.execute(&invocation, self).map(|_| ())
    }

    fn wait_for_chat_decision<T, A, P>(
        &self,
        label: &str,
        timeout_ms: u64,
        poll_ms: u64,
        accepts_message_type: A,
        parse_decision: P,
    ) -> Result<ChatDecisionWait<T>>
    where
        A: Fn(&str) -> bool,
        P: Fn(&str) -> Option<T>,
    {
        let poll_ms = poll_ms.max(50);
        let scope = if accepts_message_type("pink") {
            ChatDecisionScope::MultipleConversations
        } else {
            ChatDecisionScope::CurrentHall
        };
        let mut reader =
            self.begin_chat_decision_reader(scope, &accepts_message_type, &|text| {
                parse_decision(text).is_some()
            })?;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        while self.running.load(AtomicOrdering::SeqCst) && Instant::now() < deadline {
            workflow_actions::wait(reader.poll_interval_ms(poll_ms));
            let messages = match self.poll_chat_decision_reader(&mut reader) {
                Ok(messages) => messages,
                Err(error) => {
                    log::error!("{}扫描失败: {error:#}", label);
                    continue;
                }
            };
            for message in messages {
                if !accepts_message_type(&message.message_type) {
                    continue;
                }
                if let Some(decision) = parse_decision(&message.text) {
                    if !reader.accept_once(&message) {
                        continue;
                    }
                    return Ok(ChatDecisionWait::Found(decision));
                }
            }
        }
        if self.running.load(AtomicOrdering::SeqCst) {
            Ok(ChatDecisionWait::Timeout)
        } else {
            Ok(ChatDecisionWait::Stopped)
        }
    }

    fn execute_custom_workflow_operation(
        &mut self,
        workflow: &str,
        operation: WorkflowOperation,
    ) -> Result<()> {
        match operation {
            WorkflowOperation::Wait { duration_ms } => {
                workflow_actions::wait(duration_ms);
                Ok(())
            }
            WorkflowOperation::PressKey { key } => {
                workflow_actions::press_key_text(&key, &self.game_ui)
            }
            WorkflowOperation::HoldKey {
                key,
                duration_seconds,
            } => workflow_actions::hold_key_text(
                &key,
                duration_seconds,
                &self.game_ui,
                self.running.clone(),
            ),
            WorkflowOperation::ActivateGame { after_activate_ms } => {
                workflow_actions::activate(&self.game_ui, after_activate_ms)
            }
            WorkflowOperation::FocusGame { after_activate_ms } => {
                workflow_actions::focus(&self.game_ui, after_activate_ms)
            }
            WorkflowOperation::ClickPoint { point } => {
                workflow_actions::click_point(point_config(point), &self.game_ui)
            }
            WorkflowOperation::WaitTemplate {
                template,
                region,
                threshold,
                timeout_ms,
                poll_ms,
            } => self.execute_custom_template_operation(
                workflow,
                &template,
                rect_config(region),
                threshold,
                timeout_ms,
                poll_ms,
                None,
            ),
            WorkflowOperation::ClickTemplate {
                template,
                region,
                threshold,
                timeout_ms,
                poll_ms,
                offset,
            } => self.execute_custom_template_operation(
                workflow,
                &template,
                rect_config(region),
                threshold,
                timeout_ms,
                poll_ms,
                Some(point_config(offset)),
            ),
            WorkflowOperation::WaitTemplateAbsent {
                template,
                region,
                threshold,
                timeout_ms,
                poll_ms,
                stability,
            } => self.execute_custom_template_absent_operation(
                &template,
                rect_config(region),
                threshold,
                timeout_ms,
                poll_ms,
                stability.map(pixel_stability),
            ),
            WorkflowOperation::WaitPixelsStable {
                region,
                poll_ms,
                stability,
            } => self.execute_custom_stable_operation(
                rect_config(region),
                poll_ms,
                pixel_stability(stability),
            ),
            WorkflowOperation::WaitText {
                expected,
                region,
                timeout_ms,
                poll_ms,
            } => self.execute_custom_text_operation(
                workflow,
                &expected,
                rect_config(region),
                timeout_ms,
                poll_ms,
                None,
            ),
            WorkflowOperation::ClickText {
                expected,
                region,
                timeout_ms,
                poll_ms,
                offset,
            } => self.execute_custom_text_operation(
                workflow,
                &expected,
                rect_config(region),
                timeout_ms,
                poll_ms,
                Some(point_config(offset)),
            ),
            WorkflowOperation::PasteText {
                text,
                clipboard_hold_ms,
            } => workflow_actions::paste(&text, &self.game_ui, clipboard_hold_ms),
            WorkflowOperation::SendHall { message } => self.reply(&message),
            WorkflowOperation::SendCurrentChat { message } => {
                self.chat_output.send_current_chat(&message)
            }
            WorkflowOperation::SendFriendMessage { target, message } => {
                if self.send_friend_message(&target, &message)? {
                    Ok(())
                } else {
                    Err(anyhow!(
                        "custom workflow send_friend_message target not found: {}",
                        target
                    ))
                }
            }
            WorkflowOperation::InviteUser { target } => {
                let InviteStart::Ready(execution) =
                    self.invite.begin(InviteRequest::new(target, None, None))?
                else {
                    unreachable!("unsequenced custom workflow invites cannot be duplicates")
                };
                execution.run(self).map(|_| ())
            }
            WorkflowOperation::EnsureResidency { target } => match target {
                WorkflowResidency::Primary => {
                    self.ensure_ui_residency(UiResidency::Primary, "自定义流程要求一级界面")
                }
                WorkflowResidency::SecondaryCurrentHall => self.ensure_ui_residency(
                    UiResidency::SecondaryCurrentHall,
                    "自定义流程要求二级当前大厅",
                ),
            },
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_custom_template_operation(
        &self,
        workflow: &str,
        template: &Path,
        region: RectConfig,
        threshold: f32,
        timeout_ms: u64,
        poll_ms: u64,
        click_offset: Option<PointConfig>,
    ) -> Result<()> {
        let locator = self.ui_locator(poll_ms);
        let action = match click_offset {
            Some(offset) => HitAction::Click { offset },
            None => HitAction::Wait,
        };
        if let Some(hit) = workflow_actions::wait_or_click_template(
            &locator,
            template,
            region,
            threshold,
            timeout_ms,
            action,
            || self.running.load(AtomicOrdering::SeqCst),
        )? {
            log::info!(
                "自定义流程模板命中: workflow={} template={} score={:.3} x={} y={}",
                workflow,
                template.display(),
                hit.score,
                hit.x,
                hit.y
            );
            return Ok(());
        }
        Err(anyhow!(
            "custom workflow template not found: workflow={} template={}",
            workflow,
            template.display()
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_custom_template_absent_operation(
        &self,
        template: &Path,
        region: RectConfig,
        threshold: f32,
        timeout_ms: u64,
        poll_ms: u64,
        stability: Option<PixelStability>,
    ) -> Result<()> {
        let locator = self.ui_locator(poll_ms);
        workflow_actions::locate_template(
            &locator,
            template,
            region,
            threshold,
            timeout_ms,
            TemplateMode::Absent { stability },
            || self.running.load(AtomicOrdering::SeqCst),
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_custom_text_operation(
        &self,
        workflow: &str,
        expected: &str,
        region: RectConfig,
        timeout_ms: u64,
        poll_ms: u64,
        click_offset: Option<PointConfig>,
    ) -> Result<()> {
        let locator = self.ui_locator(poll_ms);
        let action = match click_offset {
            Some(offset) => HitAction::Click { offset },
            None => HitAction::Wait,
        };
        if let Some(point) = workflow_actions::wait_or_click_text(
            &locator,
            expected,
            region,
            timeout_ms,
            action,
            || self.running.load(AtomicOrdering::SeqCst),
            |region, expected| {
                Ok(region
                    .find_text(&self.ocr, expected)?
                    .map(|hit| hit.center()))
            },
        )? {
            log::info!(
                "自定义流程文字命中: workflow={} text={} x={} y={}",
                workflow,
                expected,
                point.x,
                point.y
            );
            return Ok(());
        }
        Err(anyhow!(
            "custom workflow text not found: workflow={} text={}",
            workflow,
            expected
        ))
    }

    fn execute_custom_stable_operation(
        &self,
        region: RectConfig,
        poll_ms: u64,
        stability: PixelStability,
    ) -> Result<()> {
        let locator = self.ui_locator(poll_ms);
        workflow_actions::wait_pixels_stable(&locator, region, stability, || {
            self.running.load(AtomicOrdering::SeqCst)
        })
    }

    fn notify_friend_invite_decision(
        &self,
        username: &str,
        message: &str,
        keep_friend_chat_open: bool,
    ) -> bool {
        let result = if keep_friend_chat_open {
            self.send_friend_message_keep_open(username, message)
        } else {
            self.send_friend_message(username, message)
        };
        match result {
            Ok(true) => true,
            Ok(false) => {
                log::error!("好友邀请确认回复失败: 未能打开好友聊天 {}", username);
                false
            }
            Err(error) => {
                log::error!("好友邀请确认回复失败: {error:#}");
                false
            }
        }
    }

    fn wait_for_invite_decision(&self) -> Result<Option<bool>> {
        match self.wait_for_chat_decision(
            "邀请确认",
            self.config.timing.invite.confirm_timeout_ms,
            self.config.timing.invite.confirm_poll_ms,
            |message_type| message_type == "blue",
            parse_invite_decision,
        )? {
            ChatDecisionWait::Found(decision) => Ok(Some(decision)),
            ChatDecisionWait::Timeout | ChatDecisionWait::Stopped => Ok(None),
        }
    }

    fn execute_invite(
        &self,
        username: &str,
        password: Option<&str>,
        friend_chat_open: bool,
    ) -> Result<bool> {
        log::info!("开始邀请: {}", username);
        let result = self.execute_invite_steps(username, password, friend_chat_open);
        if result.is_err() {
            self.return_to_primary_from_transient_ui("邀请失败");
        } else if matches!(result, Ok(true)) {
            log::info!("邀请成功，等待 10s 后兜底返回一级界面");
            workflow_actions::wait(10_000);
            self.return_to_primary_fixed();
            if let Err(error) = self.reply("BOT已经就绪,可以使用@麦克风指令了") {
                log::error!("邀请就绪消息发送失败: {error:#}");
            }
        }
        result
    }

    fn execute_invite_steps(
        &self,
        username: &str,
        password: Option<&str>,
        friend_chat_open: bool,
    ) -> Result<bool> {
        let canvas = Canvas {
            width: self.config.screen.expected_width,
            height: self.config.screen.expected_height,
            resize: true,
        };
        if friend_chat_open {
            log::info!("邀请: 已在目标好友会话，直接继续邀请步骤");
        } else if !self.open_friend_chat(
            username,
            &canvas,
            self.config.invite.friend_name_stable_count,
        )? {
            return Ok(false);
        }
        let locator = self.ui_locator_with_canvas(canvas.clone(), self.template_poll_ms());

        if !self.click_invite_target(&locator, username)? {
            log::error!("邀请失败: 确认列表未找到用户 {}", username);
            self.return_to_primary_from_transient_ui("邀请失败");
            return Ok(false);
        }

        for (label, rect, template) in [
            (
                "查看千星",
                self.config.invite.view_star_region,
                self.config.templates.invite_view_star.clone(),
            ),
            (
                "前往其大厅",
                self.config.invite.goto_hall_region,
                self.config.templates.invite_goto_hall.clone(),
            ),
            (
                "进入大厅",
                self.config.invite.enter_hall_region,
                self.config.templates.invite_enter_hall.clone(),
            ),
        ] {
            if !self.click_template_atom(
                &locator,
                &template,
                rect,
                self.config.timing.workflow.default_timeout_ms,
                label,
            )? {
                log::error!("邀请失败: 未找到{}按钮", label);
                self.return_to_primary_from_transient_ui("邀请失败");
                return Ok(false);
            }
            if label == "进入大厅" {
                if let Some(password) = password {
                    self.input_invite_password(password)?;
                }
                self.on_entered_new_hall()?;
            }
        }

        log::info!("邀请完成: {}", username);
        Ok(true)
    }

    fn input_invite_password(&self, password: &str) -> Result<()> {
        log::info!("邀请: 输入 6 位大厅密码");
        workflow_actions::wait(self.config.timing.invite.step_ms);
        for digit in password.chars() {
            workflow_actions::press_key_text(&digit.to_string(), &self.game_ui)?;
            workflow_actions::wait(self.config.timing.input.text_ms);
        }
        Ok(())
    }

    fn on_entered_new_hall(&self) -> Result<()> {
        log::info!("已进入新大厅，重置命令识别状态");
        self.abort_entertainment_for_context_loss("邀请流程已进入新大厅");
        self.commands_enabled.store(true, AtomicOrdering::SeqCst);
        self.screen_lock_primed.store(false, AtomicOrdering::SeqCst);
        self.reset_locks_requested
            .store(true, AtomicOrdering::SeqCst);
        self.clear_hall_countdown_cache_for_new_visual_session("已进入新大厅")?;
        Ok(())
    }

    pub(super) fn ui_locator(&self, poll_ms: u64) -> UiLocator {
        self.ui_locator_with_canvas(
            Canvas {
                width: self.config.screen.expected_width,
                height: self.config.screen.expected_height,
                resize: true,
            },
            poll_ms,
        )
    }

    fn ui_locator_with_canvas(&self, canvas: Canvas, poll_ms: u64) -> UiLocator {
        UiLocator::new(
            canvas,
            FrameArgs { image: None },
            self.game_ui.clone(),
            poll_ms,
        )
    }

    pub(super) fn template_poll_ms(&self) -> u64 {
        self.config.timing.input.click_ms.max(100)
    }

    pub(super) fn workflow_stability(&self, timeout_ms: u64) -> PixelStability {
        PixelStability {
            timeout_ms,
            mean_threshold: self.config.ocr.change_mean_threshold,
            changed_ratio_threshold: self.config.ocr.change_pixel_threshold,
        }
    }

    pub(super) fn wait_template_atom(
        &self,
        locator: &UiLocator,
        template: &Path,
        region: config::RectConfig,
        timeout_ms: u64,
        label: &str,
    ) -> Result<bool> {
        let hit = workflow_actions::wait_or_click_template(
            locator,
            template,
            region,
            self.config.templates.marker_threshold,
            timeout_ms,
            HitAction::Wait,
            || self.running.load(AtomicOrdering::SeqCst),
        )?;
        if hit.is_none() {
            log::error!("等待{}模板超时", label);
        }
        Ok(hit.is_some())
    }

    pub(super) fn click_template_atom(
        &self,
        locator: &UiLocator,
        template: &Path,
        region: config::RectConfig,
        timeout_ms: u64,
        label: &str,
    ) -> Result<bool> {
        let hit = workflow_actions::wait_or_click_template(
            locator,
            template,
            region,
            self.config.templates.marker_threshold,
            timeout_ms,
            HitAction::Click {
                offset: PointConfig::new(0, 0),
            },
            || self.running.load(AtomicOrdering::SeqCst),
        )?;
        if hit.is_none() {
            log::error!("等待{}模板超时", label);
        }
        Ok(hit.is_some())
    }

    fn click_invite_target(&self, locator: &UiLocator, username: &str) -> Result<bool> {
        let region = locator.region(self.config.invite.confirm_list_region.into());
        let deadline =
            Instant::now() + Duration::from_millis(self.config.timing.workflow.default_timeout_ms);
        let required_streak = self
            .config
            .resolve_stability_count(self.config.invite.friend_name_stable_count);
        let mut streak = 0_u32;
        while Instant::now() < deadline && self.running.load(AtomicOrdering::SeqCst) {
            let point = region
                .find_text_hits(&self.ocr, username)?
                .into_iter()
                .map(|hit| hit.center())
                .max_by_key(|point| point.y);
            if let Some(point) = point {
                streak = streak.saturating_add(1);
                if streak >= required_streak {
                    locator.click_point(point)?;
                    log::info!(
                        "邀请: 好友昵称完整稳定匹配，点击 {} samples={} x={} y={}",
                        username,
                        required_streak,
                        point.x,
                        point.y
                    );
                    return Ok(true);
                }
            } else {
                streak = 0;
            }
            workflow_actions::wait(locator.poll_ms());
        }
        log::error!(
            "邀请: 聊天区未稳定找到好友昵称 {} samples={}",
            username,
            required_streak
        );
        Ok(false)
    }

    pub(super) fn send_friend_message(&self, username: &str, message: &str) -> Result<bool> {
        self.send_friend_message_with_state(
            username,
            message,
            true,
            self.config.invite.friend_name_stable_count,
            true,
        )
    }

    pub(super) fn send_unique_friend_message(&self, username: &str, message: &str) -> Result<bool> {
        self.send_friend_message(username, message)
    }

    pub(super) fn send_stable_unique_friend_message(
        &self,
        username: &str,
        message: &str,
    ) -> Result<bool> {
        self.send_friend_message_with_state(
            username,
            message,
            true,
            self.config.invite.friend_name_stable_count,
            true,
        )
    }

    pub(super) fn send_secret_friend_message(&self, username: &str, message: &str) -> Result<bool> {
        self.send_friend_message_with_state(
            username,
            message,
            true,
            self.config.invite.friend_name_stable_count,
            false,
        )
    }

    fn send_friend_message_keep_open(&self, username: &str, message: &str) -> Result<bool> {
        self.send_friend_message_with_state(
            username,
            message,
            false,
            self.config.invite.friend_name_stable_count,
            true,
        )
    }

    fn send_friend_message_with_state(
        &self,
        username: &str,
        message: &str,
        restore_listener_residency: bool,
        friend_stable_count: u32,
        log_content: bool,
    ) -> Result<bool> {
        if log_content {
            log::info!("好友发言: {} -> {}", username, message);
        } else {
            log::info!("好友发言: {} -> [谁是卧底秘密内容已隐藏]", username);
        }
        let canvas = Canvas {
            width: self.config.screen.expected_width,
            height: self.config.screen.expected_height,
            resize: true,
        };
        let opened = match self.open_friend_chat(username, &canvas, friend_stable_count) {
            Ok(opened) => opened,
            Err(error) => {
                let _ = self.restore_listener_residency_after_task("好友发言失败");
                return Err(error);
            }
        };
        if !opened {
            if restore_listener_residency {
                let _ = self.restore_listener_residency_after_task("好友发言目标未找到");
            }
            return Ok(false);
        }
        // 发送反馈需要等待当前好友会话的输入框接管焦点；邀请主流程则由下一步 OCR 直接确认。
        workflow_actions::wait(self.config.timing.invite.step_ms);
        let result = self.chat_output.send_current_chat(message);
        if restore_listener_residency
            && let Err(error) = self.restore_listener_residency_after_task("好友发言")
        {
            log::error!("好友发言后恢复监听驻留界面失败: {error:#}");
        }
        result?;
        Ok(true)
    }

    fn open_friend_chat(&self, username: &str, canvas: &Canvas, stable_count: u32) -> Result<bool> {
        if !self.ensure_secondary_chat_open("打开唯一好友聊天")? {
            return Ok(false);
        }
        let locator = self.ui_locator_with_canvas(canvas.clone(), self.template_poll_ms());
        if !self.click_secondary_friend_name_atom(&locator, username, stable_count)? {
            return Ok(false);
        }
        workflow_actions::wait(self.config.timing.invite.step_ms);
        self.confirm_secondary_friend_chat_atom(&locator, username, stable_count)
    }

    fn click_secondary_friend_name_atom(
        &self,
        locator: &UiLocator,
        username: &str,
        stable_count: u32,
    ) -> Result<bool> {
        let region = locator.region(self.config.invite.friend_list_region.into());
        let deadline =
            Instant::now() + Duration::from_millis(self.config.timing.workflow.default_timeout_ms);
        let required_streak = self.config.resolve_stability_count(stable_count);
        let mut streak = 0_u32;
        while Instant::now() < deadline && self.running.load(AtomicOrdering::SeqCst) {
            let hits = region.find_text_hits(&self.ocr, username)?;
            match hits.as_slice() {
                [hit] => {
                    streak = streak.saturating_add(1);
                    if streak < required_streak {
                        log::debug!(
                            "好友昵称稳定识别: {} {}/{}",
                            username,
                            streak,
                            required_streak
                        );
                        workflow_actions::wait(locator.poll_ms());
                        continue;
                    }
                    locator.click_point(hit.center())?;
                    log::info!(
                        "原子动作完成: 二级好友昵称唯一稳定匹配 {} samples={}",
                        username,
                        required_streak
                    );
                    return Ok(true);
                }
                [] => {
                    streak = 0;
                    workflow_actions::wait(locator.poll_ms());
                }
                _ => {
                    log::error!("好友聊天失败: 昵称存在多个匹配结果 {}", username);
                    return Ok(false);
                }
            }
        }
        log::error!(
            "原子动作失败: 二级好友昵称未达到唯一稳定匹配 {} samples={}",
            username,
            required_streak
        );
        Ok(false)
    }

    fn confirm_secondary_friend_chat_atom(
        &self,
        locator: &UiLocator,
        username: &str,
        stable_count: u32,
    ) -> Result<bool> {
        let required_streak = self.config.resolve_stability_count(stable_count);
        let title_timeout_ms = self.config.timing.workflow.default_timeout_ms.min(2_000);
        let title_matched = workflow_actions::wait_latest_incoming_sender_match(
            locator,
            username,
            required_streak,
            title_timeout_ms,
            |crop| {
                self.ocr.merged_text(
                    crop.clone(),
                    self.config.ocr.same_line_y_tolerance,
                    OcrPriority::UiConfirmation,
                )
            },
            || self.running.load(AtomicOrdering::SeqCst),
        )?;
        if title_matched {
            log::info!(
                "原子动作完成: 二级好友消息标题稳定确认 {} samples={}",
                username,
                required_streak
            );
            return Ok(true);
        }
        log::info!(
            "二级好友消息标题未稳定匹配，回退聊天内容区 OCR: target={} samples={}",
            username,
            required_streak
        );

        let region = locator.region(self.config.invite.friend_chat_region.into());
        let deadline =
            Instant::now() + Duration::from_millis(self.config.timing.workflow.default_timeout_ms);
        let mut streak = 0_u32;
        while Instant::now() < deadline && self.running.load(AtomicOrdering::SeqCst) {
            let found = !region.find_text_hits(&self.ocr, username)?.is_empty();
            if found {
                streak = streak.saturating_add(1);
                if streak >= required_streak {
                    log::info!(
                        "原子动作完成: 二级聊天内容区稳定确认好友备注 {} samples={}",
                        username,
                        required_streak
                    );
                    return Ok(true);
                }
            } else {
                streak = 0;
            }
            workflow_actions::wait(locator.poll_ms());
        }
        log::error!(
            "原子动作失败: 二级聊天内容区未稳定找到好友备注 {} samples={}",
            username,
            required_streak
        );
        Ok(false)
    }
}

impl CustomWorkflowExecutionPort for AutomationApp {
    fn send_hall(&mut self, message: &str) -> Result<()> {
        self.reply(message)
    }

    fn wait_for_fresh_message(
        &mut self,
        confirmation: &WorkflowConfirmation,
        accepts_text: fn(&str) -> bool,
    ) -> Result<FreshMessageOutcome> {
        let accepts_friend_messages = confirmation.requires_multiple_conversations();
        match self.wait_for_chat_decision(
            "自定义流程确认",
            confirmation.timeout_ms,
            confirmation.poll_ms,
            |message_type| {
                if message_type == "pink" {
                    accepts_friend_messages
                } else {
                    confirmation.accepts_message_type(message_type)
                }
            },
            |text| accepts_text(text).then(|| text.to_string()),
        )? {
            ChatDecisionWait::Found(message) => Ok(FreshMessageOutcome::Message(message)),
            ChatDecisionWait::Timeout => Ok(FreshMessageOutcome::Timeout),
            ChatDecisionWait::Stopped => Ok(FreshMessageOutcome::Stopped),
        }
    }

    fn execute_operation(&mut self, workflow: &str, operation: WorkflowOperation) -> Result<()> {
        self.execute_custom_workflow_operation(workflow, operation)
    }
}

impl InviteExecutionPort for AutomationApp {
    fn is_public_hall(&self) -> Result<bool> {
        self.check_public_hall()
    }

    fn notify_friend(&self, username: &str, message: &str, keep_chat_open: bool) -> bool {
        self.notify_friend_invite_decision(username, message, keep_chat_open)
    }

    fn send_hall(&self, message: &str) -> Result<()> {
        self.reply(message)
    }

    fn wait_for_decision(&self) -> Result<InviteDecision> {
        Ok(match self.wait_for_invite_decision()? {
            Some(true) => InviteDecision::Approve,
            Some(false) => InviteDecision::Reject,
            None => InviteDecision::Timeout,
        })
    }

    fn run_invite_ui(
        &self,
        username: &str,
        password: Option<&str>,
        friend_chat_open: bool,
    ) -> Result<bool> {
        self.execute_invite(username, password, friend_chat_open)
    }
}

fn point_config(point: WorkflowPoint) -> PointConfig {
    PointConfig::new(point.x, point.y)
}

fn rect_config(rect: WorkflowRect) -> RectConfig {
    RectConfig {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
    }
}

fn pixel_stability(stability: WorkflowPixelStability) -> PixelStability {
    PixelStability {
        timeout_ms: stability.timeout_ms,
        mean_threshold: stability.mean_threshold,
        changed_ratio_threshold: stability.changed_ratio_threshold,
    }
}

fn parse_invite_decision(text: &str) -> Option<bool> {
    let raw = text.trim();
    let command_text = if let Some(index) = raw.find(['：', ':', ']', '】']) {
        let sep_len = raw[index..].chars().next().map(char::len_utf8).unwrap_or(1);
        &raw[index + sep_len..]
    } else {
        raw
    }
    .trim_start_matches(['：', ':', ' ', '\t', ']', '】']);
    if command_text
        .strip_prefix("@邀请确认")
        .is_some_and(|rest| decision_boundary(rest.chars().next()))
    {
        Some(true)
    } else if command_text
        .strip_prefix("@邀请拒绝")
        .is_some_and(|rest| decision_boundary(rest.chars().next()))
    {
        Some(false)
    } else if command_text
        .strip_prefix("@同意邀请")
        .is_some_and(|rest| decision_boundary(rest.chars().next()))
    {
        Some(true)
    } else if command_text
        .strip_prefix("@拒绝邀请")
        .is_some_and(|rest| decision_boundary(rest.chars().next()))
    {
        Some(false)
    } else {
        None
    }
}

fn decision_boundary(ch: Option<char>) -> bool {
    match ch {
        None => true,
        Some(ch) => {
            ch.is_whitespace()
                || matches!(
                    ch,
                    '，' | ',' | '。' | '.' | '!' | '！' | '?' | '？' | ']' | '】'
                )
        }
    }
}
