use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering as AtomicOrdering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::chat_listener::lowest_dark_chat_box_center;
use super::command::{self, CustomWorkflowCommand, ModerationAction, ParsedCommand, UserCommand};
use super::config::{self, CustomWorkflowConfig, CustomWorkflowDefinition, PointConfig};
use super::decision_lock::DecisionScreenLock;
use super::frame_source::{Canvas, load_frame};
use super::ui_locator::UiLocator;
use super::workflow_actions::{self, HitAction, PixelStability, TemplateMode};
use super::{
    AutomationApp, ChatDecisionScope, FrameArgs, PendingTask, PendingTaskExecution, TemplateArgs,
    TemporaryPrimaryHold, TrackedPendingTask, UiResidency,
};
use anyhow::{Result, anyhow, bail};

pub(super) fn parse_text(
    config: &CustomWorkflowConfig,
    text: &str,
    message_type: &str,
) -> Option<ParsedCommand> {
    if !config.enabled {
        return None;
    }
    let (username, command_text, user_command) = chat_command_parts(text, message_type)?;
    let (workflow, matched, args) = find_command_workflow(config, command_text, message_type)?;
    let workflow_name = workflow_name(workflow, matched);
    let raw = if args.is_empty() {
        matched.to_string()
    } else {
        format!("{} {}", matched, args)
    };
    Some(ParsedCommand {
        matched: matched.to_string(),
        raw,
        user_command,
        message_type: message_type.to_string(),
        username,
        command: UserCommand::CustomWorkflow(CustomWorkflowCommand {
            name: matched.to_string(),
            workflow: workflow_name,
            args,
        }),
    })
}

pub(super) fn find_workflow<'a>(
    config: &'a CustomWorkflowConfig,
    name: &str,
) -> Option<&'a CustomWorkflowDefinition> {
    let target = normalize_name(name);
    config
        .workflows
        .iter()
        .find(|workflow| workflow.enabled && workflow_matches_name(workflow, &target))
}

pub(super) fn template_path(config: &CustomWorkflowConfig, template: &str) -> Result<PathBuf> {
    let template = template.trim();
    if template.is_empty() {
        bail!("custom workflow template is empty");
    }
    Ok(config
        .templates
        .get(template)
        .cloned()
        .unwrap_or_else(|| PathBuf::from(template)))
}

#[derive(Clone, Debug)]
struct WorkflowContext {
    workflow: String,
    command: String,
    args: String,
    argv: Vec<String>,
    username: String,
    message_type: String,
    user_command: String,
}

impl WorkflowContext {
    fn new(command: &command::CustomWorkflowCommand, parsed: &ParsedCommand) -> Self {
        Self {
            workflow: command.workflow.clone(),
            command: command.name.clone(),
            args: command.args.clone(),
            argv: command
                .args
                .split_whitespace()
                .map(str::to_string)
                .collect(),
            username: parsed.username.clone(),
            message_type: parsed.message_type.clone(),
            user_command: parsed.user_command.clone(),
        }
    }
}

struct ModerationWorkflowRelease {
    workflows: Arc<Mutex<HashSet<String>>>,
    key: String,
}

impl ModerationWorkflowRelease {
    fn new(workflows: Arc<Mutex<HashSet<String>>>, key: String) -> Self {
        Self { workflows, key }
    }
}

impl Drop for ModerationWorkflowRelease {
    fn drop(&mut self) {
        match self.workflows.lock() {
            Ok(mut workflows) => {
                workflows.remove(&self.key);
            }
            Err(_) => {
                log::error!("moderation_workflows mutex poisoned");
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum ModerationUiState {
    OpenFriendPanel,
    OpenSearchPanel,
    EnterUid,
    WaitSearchResult,
    ClickAction,
    ConfirmAction,
    WaitActionApplied,
    Done,
}

impl ModerationUiState {
    fn label(self) -> &'static str {
        match self {
            Self::OpenFriendPanel => "打开好友界面",
            Self::OpenSearchPanel => "打开 UID 搜索",
            Self::EnterUid => "输入 UID",
            Self::WaitSearchResult => "等待搜索结果",
            Self::ClickAction => "点击执行动作",
            Self::ConfirmAction => "确认动作",
            Self::WaitActionApplied => "等待动作完成",
            Self::Done => "完成",
        }
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
        command: &command::CustomWorkflowCommand,
        parsed: &ParsedCommand,
    ) -> Result<()> {
        let workflow = find_workflow(&self.config.custom_workflows, &command.workflow)
            .cloned()
            .ok_or_else(|| anyhow!("custom workflow not found: {}", command.workflow))?;
        if workflow.steps.is_empty() {
            return Err(anyhow!(
                "custom workflow has no steps: {}",
                command.workflow
            ));
        }

        log::info!(
            "执行自定义流程: {} steps={}",
            command.workflow,
            workflow.steps.len()
        );
        let context = WorkflowContext::new(command, parsed);
        if workflow.confirm_before_run && !self.confirm_custom_workflow(&workflow, &context)? {
            log::info!("自定义流程已取消: {}", command.workflow);
            return Ok(());
        }
        for (index, step) in workflow.steps.iter().enumerate() {
            log::info!(
                "自定义流程步骤 {}/{}: {}",
                index + 1,
                workflow.steps.len(),
                step.step_type
            );
            self.execute_custom_workflow_step(&context, step)?;
            let wait_ms = if step_consumes_wait(
                step,
                self.config
                    .custom_workflows
                    .wait_template_absent_stable_default,
            ) {
                0
            } else {
                step.wait_ms
                    .unwrap_or(self.config.timing.workflow.default_step_wait_ms)
            };
            if wait_ms > 0 {
                workflow_actions::wait(wait_ms);
            }
        }
        if !workflow.success_message.trim().is_empty() {
            self.reply(&render_workflow_text(
                workflow.success_message.trim(),
                &context,
            ))?;
        }
        Ok(())
    }

    fn confirm_custom_workflow(
        &mut self,
        workflow: &CustomWorkflowDefinition,
        context: &WorkflowContext,
    ) -> Result<bool> {
        let message = if workflow.confirm_message.trim().is_empty() {
            format!(
                "{} 请求执行 {},@确认@跳过",
                context.username, context.command
            )
        } else {
            render_workflow_text(workflow.confirm_message.trim(), context)
        };
        self.reply(&message)?;
        match self.wait_for_custom_workflow_confirmation(workflow) {
            Ok(Some(true)) => Ok(true),
            Ok(Some(false)) => Ok(false),
            Ok(None) => {
                self.reply("自定义流程确认超时,已取消")?;
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    fn wait_for_custom_workflow_confirmation(
        &self,
        workflow: &CustomWorkflowDefinition,
    ) -> Result<Option<bool>> {
        let timeout_ms = workflow
            .confirm_timeout_ms
            .unwrap_or(self.config.timing.decision.timeout_ms);
        let poll_ms = workflow
            .confirm_poll_ms
            .unwrap_or(self.config.timing.decision.poll_ms)
            .max(50);
        match self.wait_for_chat_decision(
            "自定义流程确认",
            timeout_ms,
            poll_ms,
            |message_type| accepts_confirmation_message_type(workflow, message_type),
            parse_custom_workflow_confirmation,
        )? {
            ChatDecisionWait::Found(confirmed) => Ok(Some(confirmed)),
            ChatDecisionWait::Timeout => Ok(None),
            ChatDecisionWait::Stopped => Ok(Some(false)),
        }
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

    fn execute_custom_workflow_step(
        &mut self,
        context: &WorkflowContext,
        step: &config::CustomWorkflowStep,
    ) -> Result<()> {
        match step.step_type.trim() {
            "sleep" | "wait" => {
                let wait_ms = step
                    .wait_ms
                    .or(step.timeout_ms)
                    .unwrap_or(self.config.timing.workflow.default_step_wait_ms);
                workflow_actions::wait(wait_ms);
                Ok(())
            }
            "key" | "press_key" => {
                let key_text =
                    render_workflow_text(step.key.as_deref().unwrap_or("").trim(), context);
                workflow_actions::press_key_text(&key_text, &self.config.window)
            }
            "hold_key" => {
                let key_text =
                    render_workflow_text(step.key.as_deref().unwrap_or("").trim(), context);
                let hold_seconds = custom_hold_key_seconds(
                    step,
                    context,
                    self.config.custom_workflows.max_hold_key_seconds,
                )?;
                workflow_actions::hold_key_text(
                    &key_text,
                    hold_seconds,
                    &self.config.window,
                    || self.running.load(AtomicOrdering::SeqCst),
                )
            }
            "activate_game" => workflow_actions::activate(
                &self.config.window,
                self.config.timing.input.after_activate_ms,
            ),
            "focus_game" => workflow_actions::focus(
                &self.config.window,
                self.config.timing.input.after_activate_ms,
            ),
            "click" => {
                let point = step
                    .point
                    .ok_or_else(|| anyhow!("custom workflow click step missing point"))?;
                workflow_actions::click_point(point, &self.config.window)
            }
            "click_template" => self.execute_custom_template_step(context, step, true),
            "wait_template" => self.execute_custom_template_step(context, step, false),
            "wait_template_absent" => self.execute_custom_template_absent_step(context, step),
            "wait_stable" | "wait_pixels_stable" => self.execute_custom_stable_step(step),
            "click_text" => self.execute_custom_text_step(context, step, true),
            "wait_text" => self.execute_custom_text_step(context, step, false),
            "paste" | "paste_text" => {
                let text = custom_step_text(step, context);
                workflow_actions::paste(
                    &text,
                    &self.config.window,
                    self.config.timing.input.text_ms,
                )
            }
            "send_chat" | "reply" => {
                let message = custom_step_message(step, context);
                if message.is_empty() {
                    return Err(anyhow!("custom workflow send_chat step missing message"));
                }
                self.reply(&message)
            }
            "send_current_chat" => {
                let message = custom_step_message(step, context);
                if message.is_empty() {
                    return Err(anyhow!(
                        "custom workflow send_current_chat step missing message"
                    ));
                }
                self.chat_output.send_current_chat(&message)
            }
            "send_friend_message" | "friend_reply" => {
                let message = custom_step_message(step, context);
                if message.is_empty() {
                    return Err(anyhow!(
                        "custom workflow send_friend_message step missing message"
                    ));
                }
                let target = custom_step_target(step, context);
                if target.is_empty() {
                    return Err(anyhow!(
                        "custom workflow send_friend_message step missing target"
                    ));
                }
                if self.send_friend_message(&target, &message)? {
                    Ok(())
                } else {
                    Err(anyhow!(
                        "custom workflow send_friend_message target not found: {}",
                        target
                    ))
                }
            }
            "invite_user" | "invite_current_user" => {
                let target = custom_step_target(step, context);
                if target.is_empty() {
                    return Err(anyhow!("custom workflow invite step missing target"));
                }
                self.execute_invite_with_announce(&target, None).map(|_| ())
            }
            "return_primary" | "ensure_primary" => {
                self.ensure_ui_residency(UiResidency::Primary, "自定义流程要求一级界面")
            }
            "ensure_current_hall" => self.ensure_ui_residency(
                UiResidency::SecondaryCurrentHall,
                "自定义流程要求二级当前大厅",
            ),
            other => Err(anyhow!("unsupported custom workflow step type: {}", other)),
        }
    }

    fn execute_custom_template_step(
        &self,
        context: &WorkflowContext,
        step: &config::CustomWorkflowStep,
        click: bool,
    ) -> Result<()> {
        let template_name =
            render_workflow_text(step.template.as_deref().unwrap_or("").trim(), context);
        let template = template_path(&self.config.custom_workflows, &template_name)?;
        let region = step
            .region
            .ok_or_else(|| anyhow!("custom workflow template step missing region"))?;
        let threshold = step
            .threshold
            .unwrap_or(self.config.custom_workflows.default_threshold);
        let timeout_ms = step
            .timeout_ms
            .unwrap_or(self.config.timing.workflow.default_timeout_ms);
        let poll_ms = step
            .poll_ms
            .unwrap_or(self.config.timing.workflow.default_poll_ms)
            .max(50);
        let locator = self.ui_locator(poll_ms);
        let action = if click {
            HitAction::Click {
                offset: step.click_offset.unwrap_or(PointConfig::new(0, 0)),
            }
        } else {
            HitAction::Wait
        };
        if let Some(hit) = workflow_actions::wait_or_click_template(
            &locator,
            &template,
            region,
            threshold,
            timeout_ms,
            action,
            || self.running.load(AtomicOrdering::SeqCst),
        )? {
            log::info!(
                "自定义流程模板命中: workflow={} template={} score={:.3} x={} y={}",
                context.workflow,
                template_name,
                hit.score,
                hit.x,
                hit.y
            );
            return Ok(());
        }
        Err(anyhow!(
            "custom workflow template not found: workflow={} template={}",
            context.workflow,
            template_name
        ))
    }

    fn execute_custom_template_absent_step(
        &self,
        context: &WorkflowContext,
        step: &config::CustomWorkflowStep,
    ) -> Result<()> {
        let template_name =
            render_workflow_text(step.template.as_deref().unwrap_or("").trim(), context);
        let template = template_path(&self.config.custom_workflows, &template_name)?;
        let region = step
            .region
            .ok_or_else(|| anyhow!("custom workflow template step missing region"))?;
        let threshold = step
            .threshold
            .unwrap_or(self.config.custom_workflows.default_threshold);
        let timeout_ms = step
            .timeout_ms
            .unwrap_or(self.config.timing.workflow.default_timeout_ms);
        let poll_ms = step
            .poll_ms
            .unwrap_or(self.config.timing.workflow.default_poll_ms)
            .max(50);
        let locator = self.ui_locator(poll_ms);
        let stability_timeout_ms = step
            .wait_ms
            .unwrap_or(self.config.timing.workflow.default_step_wait_ms)
            .max(poll_ms);
        let stable_after_absent = step.stable_after_absent.unwrap_or(
            self.config
                .custom_workflows
                .wait_template_absent_stable_default,
        );
        workflow_actions::locate_template(
            &locator,
            &template,
            region,
            threshold,
            timeout_ms,
            TemplateMode::Absent {
                stability: stable_after_absent.then_some(PixelStability {
                    timeout_ms: stability_timeout_ms,
                    mean_threshold: self.config.ocr.change_mean_threshold,
                    changed_ratio_threshold: self.config.ocr.change_pixel_threshold,
                }),
            },
            || self.running.load(AtomicOrdering::SeqCst),
        )?;
        Ok(())
    }

    fn execute_custom_text_step(
        &self,
        context: &WorkflowContext,
        step: &config::CustomWorkflowStep,
        click: bool,
    ) -> Result<()> {
        let expected = custom_step_text(step, context);
        if expected.is_empty() {
            return Err(anyhow!("custom workflow text step missing text"));
        }
        let region = step
            .region
            .ok_or_else(|| anyhow!("custom workflow text step missing region"))?;
        let timeout_ms = step
            .timeout_ms
            .unwrap_or(self.config.timing.workflow.default_timeout_ms);
        let poll_ms = step
            .poll_ms
            .unwrap_or(self.config.timing.workflow.default_poll_ms)
            .max(50);
        let locator = self.ui_locator(poll_ms);
        let action = if click {
            HitAction::Click {
                offset: step.click_offset.unwrap_or(PointConfig::new(0, 0)),
            }
        } else {
            HitAction::Wait
        };
        if let Some(point) = workflow_actions::wait_or_click_text(
            &locator,
            &expected,
            region,
            timeout_ms,
            action,
            || self.running.load(AtomicOrdering::SeqCst),
            |region, expected| {
                let engine = self.ocr_engine()?;
                Ok(region
                    .find_text(&engine.engine, expected)?
                    .map(|hit| hit.center()))
            },
        )? {
            log::info!(
                "自定义流程文字命中: workflow={} text={} x={} y={}",
                context.workflow,
                expected,
                point.x,
                point.y
            );
            return Ok(());
        }
        Err(anyhow!(
            "custom workflow text not found: workflow={} text={}",
            context.workflow,
            expected
        ))
    }

    fn execute_custom_stable_step(&self, step: &config::CustomWorkflowStep) -> Result<()> {
        let region = step
            .region
            .ok_or_else(|| anyhow!("custom workflow wait_stable step missing region"))?;
        let timeout_ms = step
            .timeout_ms
            .or(step.wait_ms)
            .unwrap_or(self.config.timing.workflow.default_timeout_ms);
        let poll_ms = step
            .poll_ms
            .unwrap_or(self.config.timing.workflow.default_poll_ms)
            .max(50);
        let locator = self.ui_locator(poll_ms);
        workflow_actions::wait_pixels_stable(
            &locator,
            region,
            self.workflow_stability(timeout_ms),
            || self.running.load(AtomicOrdering::SeqCst),
        )
    }

    pub(super) fn execute_invite_with_announce(
        &mut self,
        username: &str,
        password: Option<&str>,
    ) -> Result<bool> {
        log::info!("邀请: 先检测是否公共大厅");
        if self.check_public_hall()? {
            log::info!("邀请: 当前在公共大厅，直接执行");
            let friend_chat_open = self.notify_friend_invite_decision(
                username,
                "已同意加入大厅,请等待BOT进入大厅并发送就绪信息后再开启麦克风",
                true,
            );
            return self.execute_invite(username, password, friend_chat_open);
        }
        let announce = format!(
            "{}邀请BOT前往大厅,30s内@邀请确认@邀请拒绝,默认通过",
            username
        );
        if let Err(error) = self.reply(&announce) {
            log::error!("邀请通告发送失败，直接执行邀请: {error:#}");
            return self.execute_invite(username, password, false);
        }
        match self.wait_for_invite_decision()? {
            Some(true) => {
                let friend_chat_open = self.notify_friend_invite_decision(
                    username,
                    "已同意加入大厅,请等待BOT进入大厅并发送就绪信息后再开启麦克风",
                    true,
                );
                self.execute_invite(username, password, friend_chat_open)
            }
            None => {
                let friend_chat_open = self.notify_friend_invite_decision(
                    username,
                    "已默认同意加入大厅,请等待BOT进入大厅并发送就绪信息后再开启麦克风",
                    true,
                );
                self.execute_invite(username, password, friend_chat_open)
            }
            Some(false) => {
                log::info!("收到邀请拒绝，取消邀请");
                self.notify_friend_invite_decision(username, "大厅成员已拒绝邀请", false);
                Ok(false)
            }
        }
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
        } else if !self.open_friend_chat(username, &canvas)? {
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
            workflow_actions::press_key_text(&digit.to_string(), &self.config.window)?;
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

    pub(super) fn execute_moderation_with_vote(
        &mut self,
        command: &command::ModerationCommand,
    ) -> Result<bool> {
        let workflow_key = moderation_workflow_key(command);
        {
            let mut workflows = self
                .moderation_workflows
                .lock()
                .map_err(|_| anyhow!("moderation_workflows mutex poisoned"))?;
            if !workflows.insert(workflow_key.clone()) {
                log::info!(
                    "{} UID{} 已有投票或执行流程，跳过重复请求",
                    command.action.label(),
                    command.uid
                );
                self.reply(&format!(
                    "@UID{}的{}请求正在处理中",
                    command.uid,
                    command.action.label()
                ))?;
                return Ok(false);
            }
        }
        let vote_timeout_seconds = self
            .config
            .timing
            .moderation
            .vote_timeout_ms
            .saturating_add(999)
            / 1000;
        let announce = format!(
            "管理员发起了对@UID{}的{}请求,请好友{}s内使用@同意/不同意进行判决",
            command.uid,
            command.action.label(),
            vote_timeout_seconds,
        );
        if let Err(error) = self.reply(&announce) {
            self.release_moderation_workflow(&workflow_key);
            return Err(error);
        }
        if let Err(error) = self.ensure_ui_residency(UiResidency::Primary, "管理投票等待前准备")
        {
            self.release_moderation_workflow(&workflow_key);
            return Err(error);
        }
        let temporary_primary_hold = match TemporaryPrimaryHold::new(self.chat_listener.clone()) {
            Ok(hold) => hold,
            Err(error) => {
                self.release_moderation_workflow(&workflow_key);
                return Err(error);
            }
        };
        self.update_monitor_chat_listener();
        self.spawn_moderation_vote(command.clone(), workflow_key, temporary_primary_hold);
        Ok(true)
    }

    fn release_moderation_workflow(&self, key: &str) {
        match self.moderation_workflows.lock() {
            Ok(mut workflows) => {
                workflows.remove(key);
            }
            Err(_) => {
                log::error!("moderation_workflows mutex poisoned");
            }
        }
    }

    fn spawn_moderation_vote(
        &self,
        command: command::ModerationCommand,
        workflow_key: String,
        temporary_primary_hold: TemporaryPrimaryHold,
    ) {
        let worker = self.clone_for_background_task();
        thread::spawn(move || {
            log::info!(
                "{} UID{} 后台投票线程已启动",
                command.action.label(),
                command.uid
            );
            let approved = match worker.wait_for_moderation_votes(&command) {
                Ok(approved) => approved,
                Err(error) => {
                    log::error!("{}后台投票失败: {error:#}", command.action.label());
                    false
                }
            };
            if !worker.running.load(AtomicOrdering::SeqCst) {
                worker.release_moderation_workflow(&workflow_key);
                drop(temporary_primary_hold);
                worker.update_monitor_chat_listener();
                return;
            }
            let task = PendingTask::ModerationVoteResult {
                command: Box::new(command),
                approved,
                workflow_key: workflow_key.clone(),
                temporary_primary_hold,
            };
            if let Err(error) = worker.push_pending_task(task) {
                log::error!("后台投票结果加入队列失败: {error:#}");
                worker.release_moderation_workflow(&workflow_key);
                worker.update_monitor_chat_listener();
            }
        });
    }

    pub(super) fn execute_moderation_vote_result(
        &mut self,
        task_id: u64,
        command: command::ModerationCommand,
        approved: bool,
        workflow_key: String,
        mut temporary_primary_hold: TemporaryPrimaryHold,
    ) -> Result<PendingTaskExecution> {
        let task_label = format!("{} UID{} 投票结果", command.action.label(), command.uid);
        match self.prepare_command_ui(&task_label) {
            Ok(true) => {}
            Ok(false) => {
                log::info!("投票结果处理前未能回到一级界面，保留任务: {}", task_label);
                let release_key = workflow_key.clone();
                let task = TrackedPendingTask {
                    id: task_id,
                    task: PendingTask::ModerationVoteResult {
                        command: Box::new(command),
                        approved,
                        workflow_key,
                        temporary_primary_hold,
                    },
                };
                if let Err(error) = self.push_pending_task_front(task) {
                    self.release_moderation_workflow(&release_key);
                    return Err(error);
                }
                return Ok(PendingTaskExecution::Requeued);
            }
            Err(error) => {
                if super::is_target_window_unavailable_error(&error) {
                    self.release_moderation_workflow(&workflow_key);
                    return Err(error);
                }
                log::error!(
                    "投票结果处理前准备界面失败，保留任务 {}: {error:#}",
                    task_label
                );
                let release_key = workflow_key.clone();
                let task = TrackedPendingTask {
                    id: task_id,
                    task: PendingTask::ModerationVoteResult {
                        command: Box::new(command),
                        approved,
                        workflow_key,
                        temporary_primary_hold,
                    },
                };
                if let Err(error) = self.push_pending_task_front(task) {
                    self.release_moderation_workflow(&release_key);
                    return Err(error);
                }
                return Ok(PendingTaskExecution::Requeued);
            }
        }

        let _workflow_release =
            ModerationWorkflowRelease::new(self.moderation_workflows.clone(), workflow_key);
        if !approved {
            temporary_primary_hold.release();
            self.update_monitor_chat_listener();
            self.reply(&format!(
                "@UID{}的{}请求未通过",
                command.uid,
                command.action.label()
            ))?;
            return Ok(PendingTaskExecution::Completed);
        }
        if let Err(error) = self.reply(&format!(
            "@UID{}的{}请求已通过,开始执行",
            command.uid,
            command.action.label()
        )) {
            temporary_primary_hold.release();
            self.update_monitor_chat_listener();
            return Err(error);
        }
        let result = self.execute_moderation_steps(&command);
        temporary_primary_hold.release();
        self.update_monitor_chat_listener();
        workflow_actions::wait(self.config.timing.command.return_retry_ms);
        match &result {
            Ok(true) => {
                if let Err(error) = self.reply(&format!(
                    "已对@UID{}执行{}",
                    command.uid,
                    command.action.label()
                )) {
                    log::error!("{}成功通告发送失败: {error:#}", command.action.label());
                }
            }
            Ok(false) | Err(_) => {
                let _ = self.reply(&format!(
                    "@UID{}的{}流程出错",
                    command.uid,
                    command.action.label()
                ));
            }
        }
        result.map(|_| PendingTaskExecution::Completed)
    }

    fn wait_for_moderation_votes(&self, command: &command::ModerationCommand) -> Result<bool> {
        let screen_lock = self.collect_moderation_vote_screen_lock();
        let deadline =
            Instant::now() + Duration::from_millis(self.config.timing.moderation.vote_timeout_ms);
        let template_args = TemplateArgs::default().resolve(&self.config);
        let canvas = Canvas {
            width: self.config.screen.expected_width,
            height: self.config.screen.expected_height,
            resize: true,
        };
        let mut stable_votes: HashMap<String, bool> = HashMap::new();
        let mut samples: HashMap<(String, bool), u32> = HashMap::new();
        while self.running.load(AtomicOrdering::SeqCst) && Instant::now() < deadline {
            workflow_actions::wait(self.config.timing.moderation.vote_poll_ms);
            let frame = match load_frame(&FrameArgs { image: None }, &canvas, &self.config.window) {
                Ok(frame) => frame,
                Err(error) => {
                    log::error!("{}投票截图失败: {error:#}", command.action.label());
                    continue;
                }
            };
            let messages = match self.scan_chat_with_shared_ocr(&frame.image, &template_args) {
                Ok(messages) => messages,
                Err(error) => {
                    log::error!("{}投票扫描失败: {error:#}", command.action.label());
                    continue;
                }
            };
            for message in messages {
                if message.message_type != "pink" || screen_lock.is_existing(&message) {
                    continue;
                }
                let Some((username, agreed)) = parse_friend_moderation_vote(&message.text) else {
                    continue;
                };
                let key = (username.clone(), agreed);
                let count = samples
                    .entry(key)
                    .and_modify(|value| *value += 1)
                    .or_insert(1);
                if *count >= self.config.moderation.stable_vote_samples {
                    stable_votes.insert(username, agreed);
                }
            }
            let agree = stable_votes.values().filter(|agreed| **agreed).count() as i32;
            let disagree = stable_votes.values().filter(|agreed| !**agreed).count() as i32;
            log::info!(
                "{}投票: 同意={} 不同意={} 差值={} 目标差值={}",
                command.action.label(),
                agree,
                disagree,
                agree - disagree,
                self.config.moderation.required_vote_margin,
            );
            if agree - disagree >= self.config.moderation.required_vote_margin {
                return Ok(true);
            }
        }
        if !self.running.load(AtomicOrdering::SeqCst) {
            return Ok(false);
        }
        let agree = stable_votes.values().filter(|agreed| **agreed).count() as i32;
        let disagree = stable_votes.values().filter(|agreed| !**agreed).count() as i32;
        if disagree == 0 {
            log::info!(
                "{}投票超时: 同意={} 不同意=0，无反对，按通过处理",
                command.action.label(),
                agree,
            );
            Ok(true)
        } else {
            log::info!(
                "{}投票超时: 同意={} 不同意={}，未达到目标差值，按未通过处理",
                command.action.label(),
                agree,
                disagree,
            );
            Ok(false)
        }
    }

    fn collect_moderation_vote_screen_lock(&self) -> DecisionScreenLock {
        let template_args = TemplateArgs::default().resolve(&self.config);
        let canvas = Canvas {
            width: self.config.screen.expected_width,
            height: self.config.screen.expected_height,
            resize: true,
        };
        let Ok(frame) = load_frame(&FrameArgs { image: None }, &canvas, &self.config.window) else {
            return DecisionScreenLock::default();
        };
        let Ok(messages) = self.scan_chat_with_shared_ocr(&frame.image, &template_args) else {
            return DecisionScreenLock::default();
        };
        DecisionScreenLock::from_messages(
            &messages,
            &|message_type| message_type == "pink",
            &|text| parse_friend_moderation_vote(text).is_some(),
        )
    }

    fn execute_moderation_steps(&self, command: &command::ModerationCommand) -> Result<bool> {
        log::info!("开始执行{} UID{}", command.action.label(), command.uid);
        let result = self.execute_moderation_steps_inner(command);
        let returned = self.return_to_primary_from_transient_ui(command.action.label());
        if matches!(result, Ok(true)) && !returned {
            log::error!(
                "{} UID{} 已执行，但返回一级界面失败，继续尝试发送成功通告",
                command.action.label(),
                command.uid
            );
        }
        result
    }

    fn execute_moderation_steps_inner(&self, command: &command::ModerationCommand) -> Result<bool> {
        self.ensure_ui_residency(UiResidency::Primary, "管理操作打开好友界面前准备")?;
        let locator = self.ui_locator(self.template_poll_ms());
        let mut state = ModerationUiState::OpenFriendPanel;

        loop {
            log::debug!(
                "{} UID{} UI 状态: {}",
                command.action.label(),
                command.uid,
                state.label()
            );

            state = match state {
                ModerationUiState::OpenFriendPanel => {
                    workflow_actions::press_key_text("o", &self.config.window)?;
                    if !self.wait_template_atom(
                        &locator,
                        &self.config.templates.friend_panel,
                        self.config.moderation.friend_panel_region,
                        self.config.timing.command.ui_timeout_ms,
                        "好友界面",
                    )? {
                        log::error!("未找到好友界面模板");
                        return Ok(false);
                    }
                    ModerationUiState::OpenSearchPanel
                }
                ModerationUiState::OpenSearchPanel => {
                    workflow_actions::press_key_text("e", &self.config.window)?;
                    workflow_actions::wait(self.config.timing.invite.step_ms);
                    workflow_actions::press_key_text("e", &self.config.window)?;
                    if !self.wait_template_atom(
                        &locator,
                        &self.config.templates.friend_search_panel,
                        self.config.moderation.search_panel_region,
                        self.config.timing.command.ui_timeout_ms,
                        "好友搜索界面",
                    )? {
                        log::error!("未找到搜索按钮模板");
                        return Ok(false);
                    }
                    ModerationUiState::EnterUid
                }
                ModerationUiState::EnterUid => {
                    log::info!(
                        "UID 搜索点击: input=({}, {}) button=({}, {})",
                        self.config.moderation.search_input_point.x,
                        self.config.moderation.search_input_point.y,
                        self.config.moderation.search_button_point.x,
                        self.config.moderation.search_button_point.y,
                    );
                    workflow_actions::click_point(
                        self.config.moderation.search_input_point,
                        &self.config.window,
                    )?;
                    workflow_actions::wait(self.config.timing.input.click_ms);
                    workflow_actions::paste(
                        &command.uid,
                        &self.config.window,
                        self.config.timing.input.text_ms,
                    )?;
                    workflow_actions::click_point(
                        self.config.moderation.search_button_point,
                        &self.config.window,
                    )?;
                    ModerationUiState::WaitSearchResult
                }
                ModerationUiState::WaitSearchResult => {
                    if !self.click_template_atom(
                        &locator,
                        &self.config.templates.friend_more_settings,
                        self.config.moderation.more_settings_region,
                        self.config.timing.moderation.search_result_timeout_ms,
                        "更多设置",
                    )? {
                        log::error!("等待更多设置模板超时");
                        return Ok(false);
                    }
                    ModerationUiState::ClickAction
                }
                ModerationUiState::ClickAction => {
                    let (region, template, label) = match command.action {
                        ModerationAction::Blacklist => (
                            self.config.moderation.blacklist_region,
                            &self.config.templates.friend_blacklist,
                            "拉黑按钮",
                        ),
                        ModerationAction::BlockChat => (
                            self.config.moderation.block_chat_region,
                            &self.config.templates.friend_block_chat,
                            "屏蔽聊天按钮",
                        ),
                    };
                    if !self.click_template_atom(
                        &locator,
                        template,
                        region,
                        self.config.timing.command.ui_timeout_ms,
                        label,
                    )? {
                        log::error!("未找到{}模板", label);
                        return Ok(false);
                    }
                    ModerationUiState::ConfirmAction
                }
                ModerationUiState::ConfirmAction => {
                    if !self.click_template_atom(
                        &locator,
                        &self.config.templates.friend_confirm,
                        self.config.moderation.confirm_region,
                        self.config.timing.command.ui_timeout_ms,
                        "确认按钮",
                    )? {
                        log::error!("未找到确认按钮模板");
                        return Ok(false);
                    }
                    ModerationUiState::WaitActionApplied
                }
                ModerationUiState::WaitActionApplied => {
                    let applied =
                        workflow_actions::locate_template(
                            &locator,
                            &self.config.templates.friend_confirm,
                            self.config.moderation.confirm_region,
                            self.config.templates.marker_threshold,
                            self.config.timing.moderation.confirm_wait_ms,
                            TemplateMode::Absent {
                                stability: Some(self.workflow_stability(
                                    self.config.timing.moderation.confirm_wait_ms,
                                )),
                            },
                            || self.running.load(AtomicOrdering::SeqCst),
                        );
                    if let Err(error) = applied {
                        log::error!("等待确认按钮模板消失超时: {error:#}");
                        return Ok(false);
                    }
                    ModerationUiState::Done
                }
                ModerationUiState::Done => {
                    log::info!("{} UID{} 完成", command.action.label(), command.uid);
                    return Ok(true);
                }
            };
        }
    }

    fn ui_locator(&self, poll_ms: u64) -> UiLocator {
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
            self.config.window.clone(),
            poll_ms,
        )
    }

    fn template_poll_ms(&self) -> u64 {
        self.config.timing.input.click_ms.max(100)
    }

    fn workflow_stability(&self, timeout_ms: u64) -> PixelStability {
        PixelStability {
            timeout_ms,
            mean_threshold: self.config.ocr.change_mean_threshold,
            changed_ratio_threshold: self.config.ocr.change_pixel_threshold,
        }
    }

    fn wait_template_atom(
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

    fn click_template_atom(
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

    fn click_text_atom(
        &self,
        locator: &UiLocator,
        expected: &str,
        region: config::RectConfig,
        timeout_ms: u64,
        label: &str,
    ) -> Result<bool> {
        let point = workflow_actions::wait_or_click_text(
            locator,
            expected,
            region,
            timeout_ms,
            HitAction::Click {
                offset: PointConfig::new(0, 0),
            },
            || self.running.load(AtomicOrdering::SeqCst),
            |region, expected| {
                let engine = self.ocr_engine()?;
                Ok(region
                    .find_text(&engine.engine, expected)?
                    .map(|hit| hit.center()))
            },
        )?;
        if point.is_none() {
            log::error!("等待{}文字超时: {}", label, expected);
        }
        Ok(point.is_some())
    }

    fn click_invite_target(&self, locator: &UiLocator, username: &str) -> Result<bool> {
        let region = self.config.invite.confirm_list_region;
        let frame = locator.capture()?;
        if let Some(point) = lowest_dark_chat_box_center(&frame.image, region.into()) {
            log::info!("邀请: 检测到最下方深色好友会话框，直接点击");
            locator.click_point(point)?;
            return Ok(true);
        }

        log::info!("邀请: 未检测到深色好友会话框，回退 OCR 查找 {}", username);
        self.click_text_atom(
            locator,
            username,
            region,
            self.config.timing.workflow.default_timeout_ms,
            "邀请确认列表用户名",
        )
    }

    pub(super) fn send_friend_message(&self, username: &str, message: &str) -> Result<bool> {
        self.send_friend_message_with_state(username, message, true, None, true)
    }

    pub(super) fn send_unique_friend_message(&self, username: &str, message: &str) -> Result<bool> {
        self.send_friend_message_with_state(username, message, true, Some(1), true)
    }

    pub(super) fn send_stable_unique_friend_message(
        &self,
        username: &str,
        message: &str,
        stable_count: u32,
    ) -> Result<bool> {
        self.send_friend_message_with_state(
            username,
            message,
            true,
            Some(stable_count.max(1)),
            true,
        )
    }

    pub(super) fn send_secret_friend_message(&self, username: &str, message: &str) -> Result<bool> {
        self.send_friend_message_with_state(username, message, true, Some(1), false)
    }

    fn send_friend_message_keep_open(&self, username: &str, message: &str) -> Result<bool> {
        self.send_friend_message_with_state(username, message, false, None, true)
    }

    fn send_friend_message_with_state(
        &self,
        username: &str,
        message: &str,
        restore_listener_residency: bool,
        unique_friend_stable_count: Option<u32>,
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
        let opened = match if let Some(stable_count) = unique_friend_stable_count {
            self.open_unique_friend_chat(username, &canvas, stable_count)
        } else {
            self.open_friend_chat(username, &canvas)
        } {
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

    fn open_unique_friend_chat(
        &self,
        username: &str,
        canvas: &Canvas,
        stable_count: u32,
    ) -> Result<bool> {
        if !self.ensure_secondary_chat_open("打开唯一好友聊天")? {
            return Ok(false);
        }
        let locator = self.ui_locator_with_canvas(canvas.clone(), self.template_poll_ms());
        let region = locator.region(self.config.invite.friend_list_region.into());
        let deadline =
            Instant::now() + Duration::from_millis(self.config.timing.workflow.default_timeout_ms);
        let required_streak = stable_count.max(1);
        let mut streak = 0_u32;
        while Instant::now() < deadline && self.running.load(AtomicOrdering::SeqCst) {
            let hits = {
                let engine = self.ocr_engine()?;
                region.find_text_hits(&engine.engine, username)?
            };
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
                    workflow_actions::wait(self.config.timing.invite.step_ms);
                    let frame = locator.capture()?;
                    let identity = self.secondary_identity_from_frame(&frame.image)?;
                    let matched = matches!(
                        identity,
                        super::chat_listener::SecondaryChatIdentity::Friend(current)
                            if {
                                let current = command::normalize_lock_text(&current);
                                let expected = command::normalize_lock_text(username);
                                !expected.is_empty()
                                    && (current.contains(&expected) || expected.contains(&current))
                            }
                    );
                    return Ok(matched);
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
        Ok(false)
    }

    fn open_friend_chat(&self, username: &str, canvas: &Canvas) -> Result<bool> {
        if !self.ensure_secondary_chat_open("打开好友聊天")? {
            log::error!("好友聊天失败: 未能打开二级聊天界面");
            return Ok(false);
        }
        let frame = load_frame(&FrameArgs { image: None }, canvas, &self.config.window)?;
        if let super::chat_listener::SecondaryChatIdentity::Friend(current) =
            self.secondary_identity_from_frame(&frame.image)?
        {
            let current = command::normalize_lock_text(&current);
            let expected = command::normalize_lock_text(username);
            if !expected.is_empty() && (current.contains(&expected) || expected.contains(&current))
            {
                log::info!("好友聊天已打开，直接复用当前会话: {}", username);
                return Ok(true);
            }
        }
        let locator = self.ui_locator_with_canvas(canvas.clone(), self.template_poll_ms());

        if !self.click_text_atom(
            &locator,
            username,
            self.config.invite.friend_list_region,
            self.config.timing.workflow.default_timeout_ms,
            "好友列表用户名",
        )? {
            log::error!("好友聊天失败: 好友列表未找到用户 {}", username);
            let _ = self.restore_listener_residency_after_task("好友聊天失败");
            return Ok(false);
        }
        Ok(true)
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

fn parse_custom_workflow_confirmation(text: &str) -> Option<bool> {
    let raw = text.trim();
    let command_text = if let Some(index) = raw.find(['：', ':', ']', '】']) {
        let sep_len = raw[index..].chars().next().map(char::len_utf8).unwrap_or(1);
        &raw[index + sep_len..]
    } else {
        raw
    }
    .trim_start_matches(['：', ':', ' ', '\t', ']', '】']);
    if command_text
        .strip_prefix("@确认")
        .is_some_and(|rest| decision_boundary(rest.chars().next()))
    {
        Some(true)
    } else if command_text
        .strip_prefix("@跳过")
        .is_some_and(|rest| decision_boundary(rest.chars().next()))
    {
        Some(false)
    } else {
        None
    }
}

fn parse_friend_moderation_vote(text: &str) -> Option<(String, bool)> {
    let sep_index = text.find(['：', ':', ']', '】'])?;
    let username = text[..sep_index]
        .trim_matches(['[', '【', ']', '】', ' ', '\t'])
        .to_string();
    if username.trim().is_empty() {
        return None;
    }
    let sep_len = text[sep_index..].chars().next()?.len_utf8();
    let command_text = text[sep_index + sep_len..]
        .trim_start_matches(['：', ':', ' ', '\t', ']', '】'])
        .strip_prefix('@')?
        .trim_start();
    if command_text
        .strip_prefix("不同意")
        .is_some_and(|rest| decision_boundary(rest.chars().next()))
    {
        Some((username, false))
    } else if command_text
        .strip_prefix("同意")
        .is_some_and(|rest| decision_boundary(rest.chars().next()))
    {
        Some((username, true))
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

fn moderation_workflow_key(command: &command::ModerationCommand) -> String {
    format!("{}:{}", command.action.label(), command.uid)
}

fn custom_step_text(step: &config::CustomWorkflowStep, context: &WorkflowContext) -> String {
    let text = step.text.as_deref().unwrap_or("").trim();
    let value = if text.is_empty() {
        step.message.as_deref().unwrap_or("").trim()
    } else {
        text
    };
    render_workflow_text(value, context)
}

fn custom_step_message(step: &config::CustomWorkflowStep, context: &WorkflowContext) -> String {
    let message = step.message.as_deref().unwrap_or("").trim();
    let value = if message.is_empty() {
        step.text.as_deref().unwrap_or("").trim()
    } else {
        message
    };
    render_workflow_text(value, context)
}

fn custom_step_target(step: &config::CustomWorkflowStep, context: &WorkflowContext) -> String {
    let target = step.target.as_deref().unwrap_or("").trim();
    if target.is_empty() {
        context.username.trim().to_string()
    } else {
        render_workflow_text(target, context)
    }
}

fn custom_hold_key_seconds(
    step: &config::CustomWorkflowStep,
    context: &WorkflowContext,
    max_seconds: u64,
) -> Result<u64> {
    if max_seconds == 0 {
        bail!("custom_workflows.max_hold_key_seconds 必须大于 0");
    }
    let argument = step.hold_seconds_arg.unwrap_or(1);
    if argument == 0 {
        bail!("hold_seconds_arg 必须从 1 开始");
    }
    let Some(raw) = context.argv.get(argument - 1) else {
        return Ok(1);
    };
    if !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("按住按键时长必须是正整数秒，最大 {} 秒", max_seconds);
    }
    let seconds = raw
        .parse::<u64>()
        .map_err(|_| anyhow!("按住按键时长无效"))?;
    if seconds == 0 || seconds > max_seconds {
        bail!("按住按键时长必须在 1 到 {} 秒之间", max_seconds);
    }
    Ok(seconds)
}

fn step_consumes_wait(step: &config::CustomWorkflowStep, stable_absent_default: bool) -> bool {
    match step.step_type.trim() {
        "sleep" | "wait" | "hold_key" => true,
        "wait_template_absent" => step.stable_after_absent.unwrap_or(stable_absent_default),
        "wait_stable" | "wait_pixels_stable" => true,
        _ => false,
    }
}

fn render_workflow_text(text: &str, context: &WorkflowContext) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        output.push_str(&rest[..start]);
        let after_start = &rest[start + 2..];
        let Some(end) = after_start.find("}}") else {
            output.push_str(&rest[start..]);
            return output;
        };
        let key = after_start[..end].trim();
        if let Some(value) = workflow_variable(key, context) {
            output.push_str(&value);
        } else {
            output.push_str("{{");
            output.push_str(&after_start[..end]);
            output.push_str("}}");
        }
        rest = &after_start[end + 2..];
    }
    output.push_str(rest);
    output
}

fn workflow_variable(key: &str, context: &WorkflowContext) -> Option<String> {
    match key {
        "workflow" | "workflow_name" => Some(context.workflow.clone()),
        "command" | "command_name" => Some(context.command.clone()),
        "args" | "param" | "params" => Some(context.args.clone()),
        "username" | "user" => Some(context.username.clone()),
        "message_type" => Some(context.message_type.clone()),
        "user_command" => Some(context.user_command.clone()),
        _ => key.strip_prefix("arg").and_then(|index| {
            let index = index.parse::<usize>().ok()?.checked_sub(1)?;
            context.argv.get(index).cloned()
        }),
    }
}

fn find_command_workflow<'a>(
    config: &'a CustomWorkflowConfig,
    command_text: &str,
    message_type: &str,
) -> Option<(&'a CustomWorkflowDefinition, &'a str, String)> {
    for workflow in config.workflows.iter().filter(|workflow| workflow.enabled) {
        if !accepts_message_type(workflow, message_type) {
            continue;
        }
        for command in &workflow.commands {
            let command = command.trim().trim_start_matches('@');
            if command.is_empty() {
                continue;
            }
            let Some(rest) = command::strip_ascii_case_prefix(command_text, command) else {
                continue;
            };
            if !command_boundary(rest.chars().next()) && !workflow.allow_args {
                continue;
            }
            let args = command_args(rest);
            if !workflow.allow_args && !args.is_empty() {
                continue;
            }
            return Some((workflow, command, args.to_string()));
        }
    }
    None
}

fn command_args(rest: &str) -> &str {
    rest.trim_start_matches(['：', ':', ' ', '\t', ']', '】'])
        .trim_end_matches([']', '】'])
        .trim()
}

fn chat_command_parts<'a>(text: &'a str, message_type: &str) -> Option<(String, &'a str, String)> {
    match message_type {
        "blue" => blue_command_parts(text),
        "pink" => pink_command_parts(text),
        _ => None,
    }
}

fn blue_command_parts(text: &str) -> Option<(String, &str, String)> {
    let sep_index = text.find(['：', ':', ']', '】'])?;
    let username = text[..sep_index]
        .trim_matches(['[', '【', ']', '】', ' ', '\t'])
        .to_string();
    let raw_command_text = after_separator(text, sep_index)?;
    let user_command = user_command_text(raw_command_text);
    let command_text = raw_command_text.strip_prefix('@')?.trim_start();
    Some((username, command_text, user_command))
}

fn pink_command_parts(text: &str) -> Option<(String, &str, String)> {
    let username = extract_bracket_username(text)?;
    let sep_index = text.find(['：', ':', ']', '】'])?;
    let raw_command_text = after_separator(text, sep_index)?;
    let user_command = user_command_text(raw_command_text);
    let command_text = raw_command_text.strip_prefix('@')?.trim_start();
    Some((username, command_text, user_command))
}

fn after_separator(text: &str, sep_index: usize) -> Option<&str> {
    let sep_len = text[sep_index..].chars().next()?.len_utf8();
    Some(text[sep_index + sep_len..].trim_start_matches(['：', ':', ' ', '\t', ']', '】']))
}

fn extract_bracket_username(text: &str) -> Option<String> {
    let (start, close) = if let Some(start) = text.find('[') {
        (start, ']')
    } else {
        (text.find('【')?, '】')
    };
    let end = text[start + 1..].find(close)? + start + 1;
    let username = text[start + 1..end].trim();
    if username.is_empty() {
        None
    } else {
        Some(username.to_string())
    }
}

fn user_command_text(text: &str) -> String {
    text.trim()
        .trim_end_matches([']', '】'])
        .trim_end()
        .to_string()
}

fn workflow_name(workflow: &CustomWorkflowDefinition, fallback: &str) -> String {
    let name = workflow.name.trim();
    if name.is_empty() {
        fallback.to_string()
    } else {
        name.to_string()
    }
}

fn workflow_matches_name(workflow: &CustomWorkflowDefinition, target: &str) -> bool {
    normalize_name(&workflow.name) == target
        || workflow
            .commands
            .iter()
            .any(|command| normalize_name(command.trim().trim_start_matches('@')) == target)
}

fn accepts_message_type(workflow: &CustomWorkflowDefinition, message_type: &str) -> bool {
    workflow.message_types.is_empty()
        || workflow
            .message_types
            .iter()
            .any(|item| item.eq_ignore_ascii_case(message_type))
}

fn accepts_confirmation_message_type(
    workflow: &CustomWorkflowDefinition,
    message_type: &str,
) -> bool {
    workflow.confirm_message_types.is_empty()
        || workflow
            .confirm_message_types
            .iter()
            .any(|item| item.eq_ignore_ascii_case(message_type))
}

fn command_boundary(ch: Option<char>) -> bool {
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

fn normalize_name(text: &str) -> String {
    text.trim().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::super::config::CustomWorkflowDefinition;
    use super::*;

    fn test_config(workflow: CustomWorkflowDefinition) -> CustomWorkflowConfig {
        CustomWorkflowConfig {
            enabled: true,
            default_threshold: 0.9,
            wait_template_absent_stable_default: true,
            max_hold_key_seconds: 10,
            templates: HashMap::new(),
            workflows: vec![workflow],
        }
    }

    fn test_workflow(allow_args: bool) -> CustomWorkflowDefinition {
        CustomWorkflowDefinition {
            enabled: true,
            name: "example".to_string(),
            commands: vec!["测试流程".to_string()],
            allow_args,
            message_types: vec!["blue".to_string()],
            confirm_before_run: false,
            confirm_message: String::new(),
            confirm_message_types: vec!["blue".to_string()],
            confirm_timeout_ms: None,
            confirm_poll_ms: None,
            steps: Vec::new(),
            success_message: String::new(),
        }
    }

    #[test]
    fn parses_blue_custom_workflow_command() {
        let config = test_config(test_workflow(false));

        let parsed = parse_text(&config, "用户：@测试流程", "blue").expect("parse custom");
        assert_eq!(parsed.matched, "测试流程");
        assert!(matches!(parsed.command, UserCommand::CustomWorkflow(_)));
    }

    #[test]
    fn parses_custom_workflow_command_case_insensitive() {
        let mut workflow = test_workflow(false);
        workflow.commands = vec!["TestFlow".to_string()];
        let config = test_config(workflow);

        let parsed =
            parse_text(&config, "用户：@testflow", "blue").expect("parse custom case insensitive");
        assert_eq!(parsed.matched, "TestFlow");
        assert!(matches!(parsed.command, UserCommand::CustomWorkflow(_)));
    }

    #[test]
    fn rejects_custom_workflow_with_extra_param() {
        let config = test_config(test_workflow(false));

        assert!(parse_text(&config, "用户：@测试流程 参数", "blue").is_none());
        assert!(parse_text(&config, "用户：@测试流程参数", "blue").is_none());
    }

    #[test]
    fn parses_custom_workflow_with_args_when_enabled() {
        let config = test_config(test_workflow(true));

        let parsed = parse_text(&config, "用户：@测试流程 123 abc", "blue").expect("parse custom");
        assert_eq!(parsed.raw, "测试流程 123 abc");
        assert!(matches!(
            parsed.command,
            UserCommand::CustomWorkflow(CustomWorkflowCommand { args, .. }) if args == "123 abc"
        ));
    }

    #[test]
    fn parses_custom_workflow_with_attached_args_when_enabled() {
        let config = test_config(test_workflow(true));

        let parsed = parse_text(&config, "用户：@测试流程123 abc", "blue").expect("parse custom");
        assert_eq!(parsed.raw, "测试流程 123 abc");
        assert!(matches!(
            parsed.command,
            UserCommand::CustomWorkflow(CustomWorkflowCommand { args, .. }) if args == "123 abc"
        ));
    }

    #[test]
    fn parses_custom_workflow_with_colon_args_when_enabled() {
        let config = test_config(test_workflow(true));

        let parsed = parse_text(&config, "用户：@测试流程：123 abc", "blue").expect("parse custom");
        assert_eq!(parsed.raw, "测试流程 123 abc");
        assert!(matches!(
            parsed.command,
            UserCommand::CustomWorkflow(CustomWorkflowCommand { args, .. }) if args == "123 abc"
        ));
    }

    #[test]
    fn renders_workflow_variables() {
        let command = CustomWorkflowCommand {
            name: "测试流程".to_string(),
            workflow: "example".to_string(),
            args: "123 abc".to_string(),
        };
        let parsed = ParsedCommand {
            matched: "测试流程".to_string(),
            raw: "测试流程 123 abc".to_string(),
            user_command: "@测试流程 123 abc".to_string(),
            message_type: "blue".to_string(),
            username: "用户".to_string(),
            command: UserCommand::CustomWorkflow(command.clone()),
        };
        let context = WorkflowContext::new(&command, &parsed);

        assert_eq!(
            render_workflow_text("{{username}} {{args}} {{arg1}} {{arg2}}", &context),
            "用户 123 abc 123 abc"
        );
    }

    #[test]
    fn hold_key_seconds_use_the_configured_argument_and_enforce_bounds() {
        let context = WorkflowContext {
            workflow: "hold-w".to_string(),
            command: "W".to_string(),
            args: "10 extra".to_string(),
            argv: vec!["10".to_string(), "extra".to_string()],
            username: "用户".to_string(),
            message_type: "pink".to_string(),
            user_command: "@W 10 extra".to_string(),
        };
        let step = config::CustomWorkflowStep {
            step_type: "hold_key".to_string(),
            template: None,
            region: None,
            point: None,
            click_offset: None,
            key: Some("W".to_string()),
            target: None,
            text: None,
            message: None,
            threshold: None,
            timeout_ms: None,
            poll_ms: None,
            wait_ms: None,
            hold_seconds_arg: Some(1),
            stable_after_absent: None,
        };

        assert_eq!(custom_hold_key_seconds(&step, &context, 10).unwrap(), 10);
        assert!(custom_hold_key_seconds(&step, &context, 9).is_err());

        let no_argument_context = WorkflowContext {
            workflow: "hold-w".to_string(),
            command: "W".to_string(),
            args: String::new(),
            argv: Vec::new(),
            username: "用户".to_string(),
            message_type: "pink".to_string(),
            user_command: "@W".to_string(),
        };
        assert_eq!(
            custom_hold_key_seconds(&step, &no_argument_context, 10).unwrap(),
            1
        );

        let mut invalid_context = WorkflowContext {
            workflow: "hold-w".to_string(),
            command: "W".to_string(),
            args: "0".to_string(),
            argv: vec!["0".to_string()],
            username: "用户".to_string(),
            message_type: "pink".to_string(),
            user_command: "@W 0".to_string(),
        };
        assert!(custom_hold_key_seconds(&step, &invalid_context, 10).is_err());

        invalid_context.args = "abc".to_string();
        invalid_context.argv = vec!["abc".to_string()];
        assert!(custom_hold_key_seconds(&step, &invalid_context, 10).is_err());
    }

    #[test]
    fn checks_confirmation_message_types() {
        let mut workflow = test_workflow(false);
        workflow.confirm_message_types = vec!["pink".to_string()];
        assert!(accepts_confirmation_message_type(&workflow, "pink"));
        assert!(!accepts_confirmation_message_type(&workflow, "blue"));

        workflow.confirm_message_types.clear();
        assert!(accepts_confirmation_message_type(&workflow, "blue"));
        assert!(accepts_confirmation_message_type(&workflow, "pink"));
    }
}
