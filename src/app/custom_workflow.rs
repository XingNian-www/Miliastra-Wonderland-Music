use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use enigo::Key;

use super::chat_scan::scan_chat;
use super::command::{self, CustomWorkflowCommand, ModerationAction, ParsedCommand, UserCommand};
use super::config::{self, CustomWorkflowConfig, CustomWorkflowDefinition, PointConfig};
use super::frame_source::{Canvas, load_frame};
use super::geometry::Point;
use super::input_actions::{click_game_point, parse_key, paste_text, press_key};
use super::template_match::best_template_hit;
use super::ui_locator::UiLocator;
use super::{AutomationApp, FrameArgs, PendingTask, TemplateArgs};

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
            let wait_ms = if matches!(step.step_type.trim(), "sleep" | "wait") {
                0
            } else {
                step.wait_ms
                    .unwrap_or(self.config.custom_workflows.default_step_wait_ms)
            };
            if wait_ms > 0 {
                sleep(Duration::from_millis(wait_ms));
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
        let existing = self.collect_custom_workflow_confirmation_bottoms(workflow);
        let timeout_ms = workflow
            .confirm_timeout_ms
            .unwrap_or(self.config.timing.decision_timeout_ms);
        let poll_ms = workflow
            .confirm_poll_ms
            .unwrap_or(self.config.timing.decision_poll_ms)
            .max(50);
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let template_args = TemplateArgs::default().resolve(&self.config);
        let canvas = Canvas {
            width: self.config.screen.expected_width,
            height: self.config.screen.expected_height,
            resize: true,
        };
        while self.running.load(AtomicOrdering::SeqCst) && Instant::now() < deadline {
            sleep(Duration::from_millis(poll_ms));
            let frame = match load_frame(&FrameArgs { image: None }, &canvas, &self.config.window) {
                Ok(frame) => frame,
                Err(error) => {
                    log::error!("自定义流程确认截图失败: {error:#}");
                    continue;
                }
            };
            let messages = {
                let engine = match self.ocr_engine() {
                    Ok(engine) => engine,
                    Err(error) => {
                        log::error!("自定义流程确认 OCR 锁失败: {error:#}");
                        continue;
                    }
                };
                match scan_chat(
                    &frame.image,
                    &engine.engine,
                    &template_args,
                    self.config.screen.chat_rect.into(),
                    Some(&self.monitor),
                ) {
                    Ok(messages) => messages,
                    Err(error) => {
                        log::error!("自定义流程确认扫描失败: {error:#}");
                        continue;
                    }
                }
            };
            for message in messages {
                if !accepts_confirmation_message_type(workflow, &message.message_type)
                    || super::is_existing_decision(&message, &existing)
                {
                    continue;
                }
                if let Some(confirmed) = parse_custom_workflow_confirmation(&message.text) {
                    return Ok(Some(confirmed));
                }
            }
        }
        if self.running.load(AtomicOrdering::SeqCst) {
            Ok(None)
        } else {
            Ok(Some(false))
        }
    }

    fn collect_custom_workflow_confirmation_bottoms(
        &self,
        workflow: &CustomWorkflowDefinition,
    ) -> HashMap<String, i32> {
        let mut output = HashMap::new();
        let template_args = TemplateArgs::default().resolve(&self.config);
        let canvas = Canvas {
            width: self.config.screen.expected_width,
            height: self.config.screen.expected_height,
            resize: true,
        };
        let Ok(frame) = load_frame(&FrameArgs { image: None }, &canvas, &self.config.window) else {
            return output;
        };
        let Ok(engine) = self.ocr_engine() else {
            return output;
        };
        let Ok(messages) = scan_chat(
            &frame.image,
            &engine.engine,
            &template_args,
            self.config.screen.chat_rect.into(),
            Some(&self.monitor),
        ) else {
            return output;
        };
        for message in messages {
            if accepts_confirmation_message_type(workflow, &message.message_type)
                && parse_custom_workflow_confirmation(&message.text).is_some()
            {
                let bottom = message.block.y + message.block.height as i32;
                output
                    .entry(message.text)
                    .and_modify(|value| *value = (*value).max(bottom))
                    .or_insert(bottom);
            }
        }
        output
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
                    .unwrap_or(self.config.custom_workflows.default_step_wait_ms);
                sleep(Duration::from_millis(wait_ms));
                Ok(())
            }
            "key" | "press_key" => {
                let key_text =
                    render_workflow_text(step.key.as_deref().unwrap_or("").trim(), context);
                if key_text.trim().is_empty() {
                    return Err(anyhow!("custom workflow step key is empty"));
                }
                let key = parse_key(&key_text)?;
                press_key(key, &self.config.window)
            }
            "click" => {
                let point = step
                    .point
                    .ok_or_else(|| anyhow!("custom workflow click step missing point"))?;
                click_game_point(point, &self.config.window)
            }
            "click_template" => self.execute_custom_template_step(context, step, true),
            "wait_template" => self.execute_custom_template_step(context, step, false),
            "wait_template_absent" => self.execute_custom_template_absent_step(context, step),
            "click_text" => self.execute_custom_text_step(context, step, true),
            "wait_text" => self.execute_custom_text_step(context, step, false),
            "paste" | "paste_text" => {
                let text = custom_step_text(step, context);
                if text.is_empty() {
                    return Err(anyhow!("custom workflow paste step missing text"));
                }
                paste_text(&text)
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
                self.execute_invite_with_announce(&target).map(|_| ())
            }
            "return_primary" => {
                self.return_to_primary_from_transient_ui(&context.workflow);
                Ok(())
            }
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
            .unwrap_or(self.config.custom_workflows.default_timeout_ms);
        let poll_ms = step
            .poll_ms
            .unwrap_or(self.config.custom_workflows.default_poll_ms)
            .max(50);
        let locator = self.ui_locator(poll_ms);
        let region = locator.region(region.into());
        if let Some(hit) = region.wait_template_while(&template, threshold, timeout_ms, || {
            self.running.load(AtomicOrdering::SeqCst)
        })? {
            log::info!(
                "自定义流程模板命中: workflow={} template={} score={:.3} x={} y={}",
                context.workflow,
                template_name,
                hit.score,
                hit.x,
                hit.y
            );
            if click {
                let center = hit.center();
                let offset = step.click_offset.unwrap_or(PointConfig::new(0, 0));
                locator.click_point(Point::new(center.x + offset.x, center.y + offset.y))?;
            }
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
            .unwrap_or(self.config.custom_workflows.default_timeout_ms);
        let poll_ms = step
            .poll_ms
            .unwrap_or(self.config.custom_workflows.default_poll_ms)
            .max(50);
        let locator = self.ui_locator(poll_ms);
        if locator.region(region.into()).wait_template_absent_while(
            &template,
            threshold,
            timeout_ms,
            || self.running.load(AtomicOrdering::SeqCst),
        )? {
            return Ok(());
        }
        Err(anyhow!(
            "custom workflow template still visible: workflow={} template={}",
            context.workflow,
            template_name
        ))
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
            .unwrap_or(self.config.custom_workflows.default_timeout_ms);
        let poll_ms = step
            .poll_ms
            .unwrap_or(self.config.custom_workflows.default_poll_ms)
            .max(50);
        let locator = self.ui_locator(poll_ms);
        let region = locator.region(region.into());
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        while self.running.load(AtomicOrdering::SeqCst) && Instant::now() <= deadline {
            let hit = {
                let engine = self.ocr_engine()?;
                region.find_text(&engine.engine, &expected)?
            };
            if let Some(hit) = hit {
                let point = hit.center();
                log::info!(
                    "自定义流程文字命中: workflow={} text={} x={} y={}",
                    context.workflow,
                    expected,
                    point.x,
                    point.y
                );
                if click {
                    let offset = step.click_offset.unwrap_or(PointConfig::new(0, 0));
                    locator.click_point(Point::new(point.x + offset.x, point.y + offset.y))?;
                }
                return Ok(());
            }
            sleep(Duration::from_millis(poll_ms));
        }
        Err(anyhow!(
            "custom workflow text not found: workflow={} text={}",
            context.workflow,
            expected
        ))
    }

    pub(super) fn execute_invite_with_announce(&mut self, username: &str) -> Result<bool> {
        log::info!("邀请: 先检测是否公共大厅");
        if self.check_public_hall()? {
            log::info!("邀请: 当前在公共大厅，直接执行");
            self.notify_friend_invite_decision(username, "已同意加入大厅,请注意启动麦克风");
            return self.execute_invite(username);
        }
        let announce = format!(
            "{}邀请BOT前往大厅,30s内@邀请确认@邀请拒绝,默认通过",
            username
        );
        if let Err(error) = self.reply(&announce) {
            log::error!("邀请通告发送失败，直接执行邀请: {error:#}");
            return self.execute_invite(username);
        }
        match self.wait_for_invite_decision()? {
            Some(true) => {
                self.notify_friend_invite_decision(username, "已同意加入大厅,请注意启动麦克风");
                self.execute_invite(username)
            }
            None => {
                self.notify_friend_invite_decision(username, "已默认同意加入大厅,请注意启动麦克风");
                self.execute_invite(username)
            }
            Some(false) => {
                log::info!("收到邀请拒绝，取消邀请");
                self.notify_friend_invite_decision(username, "大厅成员已拒绝邀请");
                self.return_to_primary_fixed();
                Ok(false)
            }
        }
    }

    fn notify_friend_invite_decision(&self, username: &str, message: &str) {
        if let Err(error) = self.send_friend_message(username, message) {
            log::error!("好友邀请确认回复失败: {error:#}");
        }
    }

    fn wait_for_invite_decision(&self) -> Result<Option<bool>> {
        let existing = self.collect_invite_decision_bottoms();
        let deadline =
            Instant::now() + Duration::from_millis(self.config.timing.invite_confirm_timeout_ms);
        let template_args = TemplateArgs::default().resolve(&self.config);
        let canvas = Canvas {
            width: self.config.screen.expected_width,
            height: self.config.screen.expected_height,
            resize: true,
        };
        while self.running.load(AtomicOrdering::SeqCst) && Instant::now() < deadline {
            sleep(Duration::from_millis(
                self.config.timing.invite_confirm_poll_ms,
            ));
            let frame = match load_frame(&FrameArgs { image: None }, &canvas, &self.config.window) {
                Ok(frame) => frame,
                Err(error) => {
                    log::error!("邀请确认截图失败: {error:#}");
                    continue;
                }
            };
            let scan_result = {
                let engine = match self.ocr_engine() {
                    Ok(engine) => engine,
                    Err(error) => {
                        log::error!("邀请确认 OCR 锁失败: {error:#}");
                        continue;
                    }
                };
                scan_chat(
                    &frame.image,
                    &engine.engine,
                    &template_args,
                    self.config.screen.chat_rect.into(),
                    Some(&self.monitor),
                )
            };
            let messages = match scan_result {
                Ok(messages) => messages,
                Err(error) => {
                    log::error!("邀请确认扫描失败: {error:#}");
                    continue;
                }
            };
            for message in messages {
                if message.message_type != "blue" {
                    continue;
                }
                if super::is_existing_decision(&message, &existing) {
                    continue;
                }
                match parse_invite_decision(&message.text) {
                    Some(true) => return Ok(Some(true)),
                    Some(false) => return Ok(Some(false)),
                    None => {}
                }
            }
        }
        Ok(None)
    }

    fn collect_invite_decision_bottoms(&self) -> HashMap<String, i32> {
        let mut output = HashMap::new();
        let template_args = TemplateArgs::default().resolve(&self.config);
        let canvas = Canvas {
            width: self.config.screen.expected_width,
            height: self.config.screen.expected_height,
            resize: true,
        };
        let Ok(frame) = load_frame(&FrameArgs { image: None }, &canvas, &self.config.window) else {
            return output;
        };
        let Ok(engine) = self.ocr_engine() else {
            return output;
        };
        let Ok(messages) = scan_chat(
            &frame.image,
            &engine.engine,
            &template_args,
            self.config.screen.chat_rect.into(),
            Some(&self.monitor),
        ) else {
            return output;
        };
        for message in messages {
            if message.message_type != "blue" {
                continue;
            }
            if parse_invite_decision(&message.text).is_some() {
                let bottom = message.block.y + message.block.height as i32;
                output
                    .entry(message.text)
                    .and_modify(|value| *value = (*value).max(bottom))
                    .or_insert(bottom);
            }
        }
        output
    }

    fn execute_invite(&self, username: &str) -> Result<bool> {
        log::info!("开始邀请: {}", username);
        let result = self.execute_invite_steps(username);
        if result.is_err() {
            self.return_to_primary_from_transient_ui("邀请失败");
        } else if matches!(result, Ok(true)) {
            log::info!("邀请成功，等待 10s 后兜底返回一级界面");
            sleep(Duration::from_secs(10));
            self.return_to_primary_fixed();
        }
        result
    }

    fn execute_invite_steps(&self, username: &str) -> Result<bool> {
        let canvas = Canvas {
            width: self.config.screen.expected_width,
            height: self.config.screen.expected_height,
            resize: true,
        };
        if !self.open_friend_chat(username, &canvas)? {
            return Ok(false);
        }
        let frame_args = FrameArgs { image: None };
        let locator = self.ui_locator_with_canvas(canvas.clone(), self.template_poll_ms());

        let clicked = {
            let engine = self.ocr_engine()?;
            locator
                .region(self.config.invite.confirm_list_region.into())
                .click_text(&engine.engine, username)?
        };
        let Some(_) = clicked else {
            log::error!("邀请失败: 确认列表未找到用户 {}", username);
            self.return_to_primary_from_transient_ui("邀请失败");
            return Ok(false);
        };
        sleep(Duration::from_millis(self.config.timing.invite_step_ms));

        for (label, rect, template) in [
            (
                "查看千星",
                self.config.invite.view_star_region.into(),
                self.config.templates.invite_view_star.clone(),
            ),
            (
                "前往其大厅",
                self.config.invite.goto_hall_region.into(),
                self.config.templates.invite_goto_hall.clone(),
            ),
            (
                "进入大厅",
                self.config.invite.enter_hall_region.into(),
                self.config.templates.invite_enter_hall.clone(),
            ),
        ] {
            let frame = load_frame(&frame_args, &canvas, &self.config.window)?;
            let Some(hit) = best_template_hit(
                &frame.image,
                Some(rect),
                &template,
                self.config.templates.marker_threshold,
            )?
            else {
                log::error!("邀请失败: 未找到{}按钮", label);
                self.return_to_primary_from_transient_ui("邀请失败");
                return Ok(false);
            };
            let center = hit.center();
            locator.click_point(center)?;
            if label == "进入大厅" {
                self.on_entered_new_hall();
            }
            sleep(Duration::from_millis(self.config.timing.invite_step_ms));
        }

        log::info!("邀请完成: {}", username);
        Ok(true)
    }

    fn on_entered_new_hall(&self) {
        log::info!("已进入新大厅，重置命令识别状态");
        self.commands_enabled.store(true, AtomicOrdering::SeqCst);
        self.screen_lock_primed.store(false, AtomicOrdering::SeqCst);
        self.reset_locks_requested
            .store(true, AtomicOrdering::SeqCst);
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
        let vote_timeout_seconds =
            self.config.moderation.vote_timeout_ms.saturating_add(999) / 1000;
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
        self.spawn_moderation_vote(command.clone(), workflow_key);
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

    fn spawn_moderation_vote(&self, command: command::ModerationCommand, workflow_key: String) {
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
                return;
            }
            let task = PendingTask::ModerationVoteResult {
                command: Box::new(command),
                approved,
                workflow_key: workflow_key.clone(),
            };
            if let Err(error) = worker.push_pending_task(task) {
                log::error!("后台投票结果加入队列失败: {error:#}");
                worker.release_moderation_workflow(&workflow_key);
            }
        });
    }

    pub(super) fn execute_moderation_vote_result(
        &mut self,
        command: command::ModerationCommand,
        approved: bool,
        workflow_key: String,
    ) -> Result<()> {
        let task_label = format!("{} UID{} 投票结果", command.action.label(), command.uid);
        match self.prepare_command_ui(&task_label) {
            Ok(true) => {}
            Ok(false) => {
                log::info!("投票结果处理前未能回到一级界面，保留任务: {}", task_label);
                self.push_pending_task_front(PendingTask::ModerationVoteResult {
                    command: Box::new(command),
                    approved,
                    workflow_key,
                })?;
                return Ok(());
            }
            Err(error) => {
                log::error!(
                    "投票结果处理前准备界面失败，保留任务 {}: {error:#}",
                    task_label
                );
                self.push_pending_task_front(PendingTask::ModerationVoteResult {
                    command: Box::new(command),
                    approved,
                    workflow_key,
                })?;
                return Ok(());
            }
        }

        let _workflow_release =
            ModerationWorkflowRelease::new(self.moderation_workflows.clone(), workflow_key);
        if !approved {
            self.reply(&format!(
                "@UID{}的{}请求未通过",
                command.uid,
                command.action.label()
            ))?;
            return Ok(());
        }
        self.reply(&format!(
            "@UID{}的{}请求已通过,开始执行",
            command.uid,
            command.action.label()
        ))?;
        let result = self.execute_moderation_steps(&command);
        sleep(Duration::from_millis(
            self.config.timing.return_to_primary_retry_ms,
        ));
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
        result.map(|_| ())
    }

    fn wait_for_moderation_votes(&self, command: &command::ModerationCommand) -> Result<bool> {
        let existing = self.collect_moderation_vote_bottoms();
        let deadline =
            Instant::now() + Duration::from_millis(self.config.moderation.vote_timeout_ms);
        let template_args = TemplateArgs::default().resolve(&self.config);
        let canvas = Canvas {
            width: self.config.screen.expected_width,
            height: self.config.screen.expected_height,
            resize: true,
        };
        let mut stable_votes: HashMap<String, bool> = HashMap::new();
        let mut samples: HashMap<(String, bool), u32> = HashMap::new();
        while self.running.load(AtomicOrdering::SeqCst) && Instant::now() < deadline {
            sleep(Duration::from_millis(self.config.moderation.vote_poll_ms));
            let frame = match load_frame(&FrameArgs { image: None }, &canvas, &self.config.window) {
                Ok(frame) => frame,
                Err(error) => {
                    log::error!("{}投票截图失败: {error:#}", command.action.label());
                    continue;
                }
            };
            let messages = {
                let engine = match self.ocr_engine() {
                    Ok(engine) => engine,
                    Err(error) => {
                        log::error!("{}投票 OCR 锁失败: {error:#}", command.action.label());
                        continue;
                    }
                };
                match scan_chat(
                    &frame.image,
                    &engine.engine,
                    &template_args,
                    self.config.screen.chat_rect.into(),
                    Some(&self.monitor),
                ) {
                    Ok(messages) => messages,
                    Err(error) => {
                        log::error!("{}投票扫描失败: {error:#}", command.action.label());
                        continue;
                    }
                }
            };
            for message in messages {
                if message.message_type != "pink"
                    || super::is_existing_decision(&message, &existing)
                {
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

    fn collect_moderation_vote_bottoms(&self) -> HashMap<String, i32> {
        let mut output = HashMap::new();
        let template_args = TemplateArgs::default().resolve(&self.config);
        let canvas = Canvas {
            width: self.config.screen.expected_width,
            height: self.config.screen.expected_height,
            resize: true,
        };
        let Ok(frame) = load_frame(&FrameArgs { image: None }, &canvas, &self.config.window) else {
            return output;
        };
        let Ok(engine) = self.ocr_engine() else {
            return output;
        };
        let Ok(messages) = scan_chat(
            &frame.image,
            &engine.engine,
            &template_args,
            self.config.screen.chat_rect.into(),
            Some(&self.monitor),
        ) else {
            return output;
        };
        for message in messages {
            if message.message_type == "pink"
                && parse_friend_moderation_vote(&message.text).is_some()
            {
                let bottom = message.block.y + message.block.height as i32;
                output
                    .entry(message.text)
                    .and_modify(|value| *value = (*value).max(bottom))
                    .or_insert(bottom);
            }
        }
        output
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
                    press_key(Key::Unicode('o'), &self.config.window)?;
                    sleep(Duration::from_millis(self.config.timing.invite_step_ms));
                    if locator
                        .region(self.config.moderation.friend_panel_region.into())
                        .find_template(&self.config.templates.friend_panel)?
                        .is_none()
                    {
                        log::error!("未找到好友界面模板");
                        return Ok(false);
                    }
                    ModerationUiState::OpenSearchPanel
                }
                ModerationUiState::OpenSearchPanel => {
                    press_key(Key::Unicode('e'), &self.config.window)?;
                    sleep(Duration::from_millis(self.config.timing.invite_step_ms));
                    press_key(Key::Unicode('e'), &self.config.window)?;
                    sleep(Duration::from_millis(self.config.timing.invite_step_ms));
                    if locator
                        .region(self.config.moderation.search_panel_region.into())
                        .find_template(&self.config.templates.friend_search_panel)?
                        .is_none()
                    {
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
                    locator.click_point(Point::new(
                        self.config.moderation.search_input_point.x,
                        self.config.moderation.search_input_point.y,
                    ))?;
                    sleep(Duration::from_millis(self.config.timing.output_click_ms));
                    paste_text(&command.uid)?;
                    sleep(Duration::from_millis(self.config.timing.output_input_ms));
                    locator.click_point(Point::new(
                        self.config.moderation.search_button_point.x,
                        self.config.moderation.search_button_point.y,
                    ))?;
                    ModerationUiState::WaitSearchResult
                }
                ModerationUiState::WaitSearchResult => {
                    let Some(hit) = locator
                        .region(self.config.moderation.more_settings_region.into())
                        .wait_template_while(
                            &self.config.templates.friend_more_settings,
                            self.config.templates.marker_threshold,
                            self.config.moderation.search_result_timeout_ms,
                            || self.running.load(AtomicOrdering::SeqCst),
                        )?
                    else {
                        log::error!("等待更多设置模板超时");
                        return Ok(false);
                    };
                    locator.click_point(hit.center())?;
                    sleep(Duration::from_millis(self.config.timing.invite_step_ms));
                    ModerationUiState::ClickAction
                }
                ModerationUiState::ClickAction => {
                    let (region, template, label) = match command.action {
                        ModerationAction::Blacklist => (
                            self.config.moderation.blacklist_region.into(),
                            &self.config.templates.friend_blacklist,
                            "拉黑按钮",
                        ),
                        ModerationAction::BlockChat => (
                            self.config.moderation.block_chat_region.into(),
                            &self.config.templates.friend_block_chat,
                            "屏蔽聊天按钮",
                        ),
                    };
                    if locator.region(region).click_template(template)?.is_none() {
                        log::error!("未找到{}模板", label);
                        return Ok(false);
                    }
                    sleep(Duration::from_millis(self.config.timing.invite_step_ms));
                    ModerationUiState::ConfirmAction
                }
                ModerationUiState::ConfirmAction => {
                    if locator
                        .region(self.config.moderation.confirm_region.into())
                        .click_template(&self.config.templates.friend_confirm)?
                        .is_none()
                    {
                        log::error!("未找到确认按钮模板");
                        return Ok(false);
                    }
                    ModerationUiState::WaitActionApplied
                }
                ModerationUiState::WaitActionApplied => {
                    if !locator
                        .region(self.config.moderation.confirm_region.into())
                        .wait_template_absent_while(
                            &self.config.templates.friend_confirm,
                            self.config.templates.marker_threshold,
                            self.config.moderation.confirm_wait_ms,
                            || self.running.load(AtomicOrdering::SeqCst),
                        )?
                    {
                        log::error!("等待确认按钮模板消失超时");
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
            self.config.templates.marker_threshold,
            poll_ms,
        )
    }

    fn template_poll_ms(&self) -> u64 {
        self.config.timing.output_click_ms.max(100)
    }

    fn send_friend_message(&self, username: &str, message: &str) -> Result<bool> {
        log::info!("好友发言: {} -> {}", username, message);
        let canvas = Canvas {
            width: self.config.screen.expected_width,
            height: self.config.screen.expected_height,
            resize: true,
        };
        let opened = match self.open_friend_chat(username, &canvas) {
            Ok(opened) => opened,
            Err(error) => {
                self.return_to_primary_from_transient_ui("好友发言失败");
                return Err(error);
            }
        };
        if !opened {
            return Ok(false);
        }
        let result = self.chat_output.send_current_chat(message);
        self.return_to_primary_from_transient_ui("好友发言");
        result?;
        Ok(true)
    }

    fn open_friend_chat(&self, username: &str, canvas: &Canvas) -> Result<bool> {
        click_game_point(self.config.output.focus_point, &self.config.window)?;
        sleep(Duration::from_millis(
            self.config.timing.invite_open_chat_ms,
        ));
        press_key(Key::Return, &self.config.window)?;
        sleep(Duration::from_millis(
            self.config.timing.invite_open_chat_ms,
        ));
        let locator = self.ui_locator_with_canvas(canvas.clone(), self.template_poll_ms());

        let clicked = {
            let engine = self.ocr_engine()?;
            locator
                .region(self.config.invite.friend_list_region.into())
                .click_text(&engine.engine, username)?
        };
        let Some(_) = clicked else {
            log::error!("好友聊天失败: 好友列表未找到用户 {}", username);
            self.return_to_primary_from_transient_ui("好友聊天失败");
            return Ok(false);
        };
        sleep(Duration::from_millis(self.config.timing.invite_step_ms));
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
            default_timeout_ms: 5_000,
            default_poll_ms: 200,
            default_step_wait_ms: 300,
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
