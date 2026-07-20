use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::config::{PointConfig, RectConfig};
use crate::features::command::{CommandEnvelope, CommandPrefix, FeatureCommandMatch};
pub use crate::interfaces::ui_plan::{
    WorkflowMouseButton, WorkflowOperation, WorkflowPixelStability, WorkflowPoint, WorkflowRect,
    WorkflowResidency,
};
use crate::text::normalize_comparison_text;

const MIN_POLL_MS: u64 = 50;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomWorkflowConfig {
    pub enabled: bool,
    pub default_threshold: f32,
    pub wait_template_absent_stable_default: bool,
    pub max_hold_key_seconds: u64,
    pub templates: HashMap<String, PathBuf>,
    pub workflows: Vec<CustomWorkflowDefinition>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowTimingConfig {
    pub default_timeout_ms: u64,
    pub default_poll_ms: u64,
    pub default_step_wait_ms: u64,
}

impl WorkflowTimingConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.default_timeout_ms == 0 {
            bail!("timing.workflow.default_timeout_ms 必须大于 0");
        }
        if self.default_poll_ms == 0 {
            bail!("timing.workflow.default_poll_ms 必须大于 0");
        }
        Ok(())
    }
}

impl CustomWorkflowConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if !self.default_threshold.is_finite() || !(0.0..=1.0).contains(&self.default_threshold) {
            bail!("custom_workflows.default_threshold 必须是 0 到 1 之间的有限小数");
        }
        if self.max_hold_key_seconds == 0 {
            bail!("custom_workflows.max_hold_key_seconds 必须大于 0");
        }
        for workflow in self.workflows.iter().filter(|workflow| workflow.enabled) {
            if workflow.steps.is_empty() {
                bail!("custom workflow has no steps: {}", workflow.name.trim());
            }
            for step in &workflow.steps {
                validate_configured_step(step)?;
            }
        }
        Ok(())
    }
}

fn validate_configured_step(step: &CustomWorkflowStep) -> Result<()> {
    let step_type = step.step_type.trim();
    if !supported_step_type(step_type) {
        bail!("unsupported custom workflow step type: {}", step_type);
    }
    if let Some(threshold) = step.threshold
        && (!threshold.is_finite() || !(0.0..=1.0).contains(&threshold))
    {
        bail!("custom workflow step threshold must be between 0 and 1");
    }

    match step_type {
        "key" | "press_key" if configured_text(step.key.as_deref()).is_empty() => {
            bail!("custom workflow step key is empty")
        }
        "hold_key" => {
            if configured_text(step.key.as_deref()).is_empty() {
                bail!("自定义流程按住按键缺少 key");
            }
            if step.hold_seconds_arg == Some(0) {
                bail!("hold_seconds_arg 必须从 1 开始");
            }
        }
        "click" if step.point.is_none() => {
            bail!("custom workflow click step missing point")
        }
        "mouse_button" => {
            parse_mouse_button(configured_text(step.button.as_deref()))?;
        }
        "click_template" | "wait_template" | "wait_template_absent" => {
            if configured_text(step.template.as_deref()).is_empty() {
                bail!("custom workflow template is empty");
            }
            validate_configured_region(step, "custom workflow template step missing region")?;
        }
        "wait_stable" | "wait_pixels_stable" => {
            validate_configured_region(step, "custom workflow wait_stable step missing region")?;
        }
        "click_text" | "wait_text" if configured_step_text(step).is_empty() => {
            bail!("custom workflow text step missing text")
        }
        "click_text" | "wait_text" => {
            validate_configured_region(step, "custom workflow text step missing region")?;
        }
        "paste" | "paste_text" if configured_step_text(step).is_empty() => {
            bail!("custom workflow paste step missing text")
        }
        "send_chat" | "reply" if configured_step_message(step).is_empty() => {
            bail!("custom workflow send_chat step missing message")
        }
        "send_current_chat" if configured_step_message(step).is_empty() => {
            bail!("custom workflow send_current_chat step missing message")
        }
        "send_friend_message" | "friend_reply" if configured_step_message(step).is_empty() => {
            bail!("custom workflow send_friend_message step missing message")
        }
        _ => {}
    }
    Ok(())
}

fn validate_configured_region(step: &CustomWorkflowStep, missing: &str) -> Result<()> {
    let region = step.region.ok_or_else(|| anyhow!(missing.to_string()))?;
    if region.width == 0 || region.height == 0 {
        bail!("custom workflow step region must have positive width and height");
    }
    Ok(())
}

fn configured_text(value: Option<&str>) -> &str {
    value.unwrap_or("").trim()
}

fn configured_step_text(step: &CustomWorkflowStep) -> &str {
    let text = configured_text(step.text.as_deref());
    if text.is_empty() {
        configured_text(step.message.as_deref())
    } else {
        text
    }
}

fn configured_step_message(step: &CustomWorkflowStep) -> &str {
    let message = configured_text(step.message.as_deref());
    if message.is_empty() {
        configured_text(step.text.as_deref())
    } else {
        message
    }
}

fn supported_step_type(step_type: &str) -> bool {
    matches!(
        step_type,
        "sleep"
            | "wait"
            | "key"
            | "press_key"
            | "hold_key"
            | "activate_game"
            | "focus_game"
            | "click"
            | "mouse_button"
            | "click_template"
            | "wait_template"
            | "wait_template_absent"
            | "wait_stable"
            | "wait_pixels_stable"
            | "click_text"
            | "wait_text"
            | "paste"
            | "paste_text"
            | "send_chat"
            | "reply"
            | "send_current_chat"
            | "send_friend_message"
            | "friend_reply"
            | "invite_user"
            | "invite_current_user"
            | "return_primary"
            | "ensure_primary"
            | "ensure_current_hall"
    )
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomWorkflowDefinition {
    pub enabled: bool,
    pub name: String,
    pub commands: Vec<String>,
    pub allow_args: bool,
    pub message_types: Vec<String>,
    pub confirm_before_run: bool,
    pub confirm_message: String,
    pub confirm_message_types: Vec<String>,
    pub confirm_timeout_ms: Option<u64>,
    pub confirm_poll_ms: Option<u64>,
    pub steps: Vec<CustomWorkflowStep>,
    pub success_message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomWorkflowStep {
    #[serde(rename = "type")]
    pub step_type: String,
    pub template: Option<String>,
    pub region: Option<RectConfig>,
    pub point: Option<PointConfig>,
    pub click_offset: Option<PointConfig>,
    pub key: Option<String>,
    pub button: Option<String>,
    pub target: Option<String>,
    pub text: Option<String>,
    pub message: Option<String>,
    pub threshold: Option<f32>,
    pub timeout_ms: Option<u64>,
    pub poll_ms: Option<u64>,
    pub wait_ms: Option<u64>,
    pub hold_seconds_arg: Option<usize>,
    pub stable_after_absent: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CustomWorkflowCommand {
    pub name: String,
    pub workflow: String,
    pub args: String,
}

impl CustomWorkflowCommand {
    pub fn lock_identity(&self) -> WorkflowLockIdentity {
        WorkflowLockIdentity {
            workflow: identity_text(&self.workflow),
            args: identity_text(&self.args),
        }
    }

    pub fn lock_key(&self) -> String {
        let identity = self.lock_identity();
        format!("custom_workflow:{}:{}", identity.workflow, identity.args)
    }

    pub fn same_request(&self, other: &Self) -> bool {
        self.lock_identity() == other.lock_identity()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowLockIdentity {
    pub workflow: String,
    pub args: String,
}

pub type CustomWorkflowMatch = FeatureCommandMatch<CustomWorkflowCommand>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomWorkflowInvocation {
    pub command: CustomWorkflowCommand,
    pub username: String,
    pub message_type: String,
    pub user_command: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomWorkflowSummary {
    pub name: String,
    pub commands: Vec<String>,
    pub allow_args: bool,
    pub confirm_before_run: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WorkflowDefaults {
    pub default_timeout_ms: u64,
    pub default_poll_ms: u64,
    pub default_step_wait_ms: u64,
    pub decision_timeout_ms: u64,
    pub decision_poll_ms: u64,
    pub after_activate_ms: u64,
    pub clipboard_hold_ms: u64,
    pub stability_mean_threshold: f32,
    pub stability_changed_ratio_threshold: f32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FreshMessageOutcome {
    Message(String),
    Timeout,
    Stopped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkflowCompletion {
    Completed,
    Cancelled,
}

pub trait CustomWorkflowExecutionPort {
    fn send_hall(&mut self, message: &str) -> Result<()>;

    fn wait_for_fresh_message(
        &mut self,
        confirmation: &WorkflowConfirmation,
        accepts_text: fn(&str) -> bool,
    ) -> Result<FreshMessageOutcome>;

    fn execute_action_plan(
        &mut self,
        workflow: &str,
        operations: Vec<WorkflowOperation>,
    ) -> Result<()>;

    fn execute_capability(&mut self, workflow: &str, capability: WorkflowCapability) -> Result<()>;
}

#[derive(Clone, Debug)]
pub struct CustomWorkflowService {
    config: CustomWorkflowConfig,
    defaults: WorkflowDefaults,
}

impl CustomWorkflowService {
    pub fn new(config: CustomWorkflowConfig, defaults: WorkflowDefaults) -> Self {
        Self { config, defaults }
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    pub fn claims_chat(&self, envelope: &CommandEnvelope) -> bool {
        if !self.config.enabled || envelope.prefix() != CommandPrefix::At {
            return false;
        }
        find_command_workflow(
            &self.config,
            envelope.command_text(),
            envelope.message_type(),
        )
        .is_some()
    }

    pub fn parse_chat(&self, envelope: &CommandEnvelope) -> Option<CustomWorkflowMatch> {
        if !self.config.enabled || envelope.prefix() != CommandPrefix::At {
            return None;
        }
        let (workflow, matched, args) = find_command_workflow(
            &self.config,
            envelope.command_text(),
            envelope.message_type(),
        )?;
        let workflow_name = workflow_name(workflow, matched);
        Some(FeatureCommandMatch::new(
            matched,
            joined_command(matched, &args),
            CustomWorkflowCommand {
                name: matched.to_string(),
                workflow: workflow_name,
                args,
            },
        ))
    }

    fn find(&self, name: &str) -> Option<&CustomWorkflowDefinition> {
        find_workflow(&self.config, name)
    }

    pub fn list(&self) -> Vec<CustomWorkflowSummary> {
        self.config
            .workflows
            .iter()
            .filter(|workflow| workflow.enabled)
            .filter_map(|workflow| {
                let name = if workflow.name.trim().is_empty() {
                    workflow.commands.first()?.trim()
                } else {
                    workflow.name.trim()
                };
                Some(CustomWorkflowSummary {
                    name: name.to_string(),
                    commands: workflow.commands.clone(),
                    allow_args: workflow.allow_args,
                    confirm_before_run: workflow.confirm_before_run,
                })
            })
            .collect()
    }

    pub fn prepare_remote(&self, name: &str, args: &str) -> Result<CustomWorkflowMatch> {
        if !self.config.enabled {
            bail!("自定义工作流未启用");
        }
        let name = name.trim();
        if name.is_empty() {
            bail!("name 不能为空");
        }
        let args = args.trim();
        let workflow = self
            .find(name)
            .ok_or_else(|| anyhow!("自定义工作流不存在或未启用"))?;
        if !workflow.allow_args && !args.is_empty() {
            bail!("该自定义工作流不允许参数");
        }
        let command_name = workflow
            .commands
            .first()
            .map(|command| command.trim().trim_start_matches('@'))
            .filter(|command| !command.is_empty())
            .unwrap_or(name)
            .to_string();
        let workflow_name = workflow_name(workflow, &command_name);
        let raw = joined_command(&command_name, args);
        Ok(FeatureCommandMatch::new(
            command_name.clone(),
            raw,
            CustomWorkflowCommand {
                name: command_name,
                workflow: workflow_name,
                args: args.to_string(),
            },
        ))
    }

    pub fn execute(
        &self,
        invocation: &CustomWorkflowInvocation,
        port: &mut dyn CustomWorkflowExecutionPort,
    ) -> Result<WorkflowCompletion> {
        let plan = self.prepare(invocation)?;
        log::info!(
            "执行自定义流程: {} steps={}",
            plan.workflow,
            plan.steps.len()
        );
        if let Some(confirmation) = &plan.confirmation {
            port.send_hall(&confirmation.message)?;
            let decision =
                match port.wait_for_fresh_message(confirmation, is_confirmation_message)? {
                    FreshMessageOutcome::Message(message) => parse_confirmation(&message),
                    FreshMessageOutcome::Timeout => {
                        port.send_hall("自定义流程确认超时,已取消")?;
                        None
                    }
                    FreshMessageOutcome::Stopped => None,
                };
            if decision != Some(WorkflowConfirmationDecision::Confirm) {
                log::info!("自定义流程已取消: {}", plan.workflow);
                return Ok(WorkflowCompletion::Cancelled);
            }
        }

        let step_count = plan.steps.len();
        for (index, step) in plan.steps.into_iter().enumerate() {
            match step {
                WorkflowPlanStep::ActionPlan(operations) => {
                    log::info!(
                        "自定义流程机械段 {}/{}: operations={}",
                        index + 1,
                        step_count,
                        operations.len()
                    );
                    port.execute_action_plan(&plan.workflow, operations)?;
                }
                WorkflowPlanStep::Capability(capability) => {
                    log::info!(
                        "自定义流程能力步骤 {}/{}: {}",
                        index + 1,
                        step_count,
                        capability_label(&capability)
                    );
                    port.execute_capability(&plan.workflow, capability)?;
                }
            }
        }
        if let Some(message) = plan.success_message {
            port.send_hall(&message)?;
        }
        Ok(WorkflowCompletion::Completed)
    }

    fn prepare(&self, invocation: &CustomWorkflowInvocation) -> Result<WorkflowPlan> {
        let workflow = self
            .find(&invocation.command.workflow)
            .ok_or_else(|| anyhow!("custom workflow not found: {}", invocation.command.workflow))?;
        if workflow.steps.is_empty() {
            bail!(
                "custom workflow has no steps: {}",
                invocation.command.workflow
            );
        }

        let context = WorkflowContext::new(invocation);
        let confirmation = workflow
            .confirm_before_run
            .then(|| self.prepare_confirmation(workflow, &context));
        let mut steps = Vec::with_capacity(workflow.steps.len().saturating_mul(2));
        for step in &workflow.steps {
            steps.push(self.prepare_step(step, &context)?);
            if !step_consumes_wait(step, self.config.wait_template_absent_stable_default) {
                let wait_ms = step.wait_ms.unwrap_or(self.defaults.default_step_wait_ms);
                if wait_ms > 0 {
                    steps.push(PreparedWorkflowStep::Mechanical(WorkflowOperation::Wait {
                        duration_ms: wait_ms,
                    }));
                }
            }
        }
        let success_message = workflow.success_message.trim();
        let success_message =
            (!success_message.is_empty()).then(|| context.render(success_message));

        Ok(WorkflowPlan {
            workflow: invocation.command.workflow.clone(),
            confirmation,
            steps: compile_plan_steps(steps),
            success_message,
        })
    }

    fn prepare_confirmation(
        &self,
        workflow: &CustomWorkflowDefinition,
        context: &WorkflowContext,
    ) -> WorkflowConfirmation {
        let message = if workflow.confirm_message.trim().is_empty() {
            format!(
                "{} 请求执行 {},@确认@跳过",
                context.username, context.command
            )
        } else {
            context.render(workflow.confirm_message.trim())
        };
        WorkflowConfirmation {
            message,
            message_types: workflow.confirm_message_types.clone(),
            timeout_ms: workflow
                .confirm_timeout_ms
                .unwrap_or(self.defaults.decision_timeout_ms),
            poll_ms: workflow
                .confirm_poll_ms
                .unwrap_or(self.defaults.decision_poll_ms)
                .max(MIN_POLL_MS),
        }
    }

    fn prepare_step(
        &self,
        step: &CustomWorkflowStep,
        context: &WorkflowContext,
    ) -> Result<PreparedWorkflowStep> {
        match step.step_type.trim() {
            "sleep" | "wait" => Ok(PreparedWorkflowStep::Mechanical(WorkflowOperation::Wait {
                duration_ms: step
                    .wait_ms
                    .or(step.timeout_ms)
                    .unwrap_or(self.defaults.default_step_wait_ms),
            })),
            "key" | "press_key" => {
                let key = context.render(step.key.as_deref().unwrap_or("").trim());
                if key.trim().is_empty() {
                    bail!("custom workflow step key is empty");
                }
                Ok(PreparedWorkflowStep::Mechanical(
                    WorkflowOperation::PressKey { key },
                ))
            }
            "hold_key" => {
                let key = context.render(step.key.as_deref().unwrap_or("").trim());
                if key.trim().is_empty() {
                    bail!("自定义流程按住按键缺少 key");
                }
                Ok(PreparedWorkflowStep::Mechanical(
                    WorkflowOperation::HoldKey {
                        key,
                        duration_seconds: custom_hold_key_seconds(
                            step,
                            context,
                            self.config.max_hold_key_seconds,
                        )?,
                    },
                ))
            }
            "activate_game" => Ok(PreparedWorkflowStep::Mechanical(
                WorkflowOperation::ActivateGame {
                    after_activate_ms: self.defaults.after_activate_ms,
                },
            )),
            "focus_game" => Ok(PreparedWorkflowStep::Mechanical(
                WorkflowOperation::FocusGame {
                    after_activate_ms: self.defaults.after_activate_ms,
                },
            )),
            "click" => Ok(PreparedWorkflowStep::Mechanical(
                WorkflowOperation::ClickPoint {
                    point: step
                        .point
                        .ok_or_else(|| anyhow!("custom workflow click step missing point"))?
                        .into(),
                },
            )),
            "mouse_button" => Ok(PreparedWorkflowStep::Mechanical(
                WorkflowOperation::ClickMouseButton {
                    button: parse_mouse_button(configured_text(step.button.as_deref()))?,
                },
            )),
            "click_template" => self.prepare_template_step(step, context, true),
            "wait_template" => self.prepare_template_step(step, context, false),
            "wait_template_absent" => self.prepare_template_absent_step(step, context),
            "wait_stable" | "wait_pixels_stable" => {
                let region =
                    required_region(step, "custom workflow wait_stable step missing region")?;
                let timeout_ms = step
                    .timeout_ms
                    .or(step.wait_ms)
                    .unwrap_or(self.defaults.default_timeout_ms);
                Ok(PreparedWorkflowStep::Mechanical(
                    WorkflowOperation::WaitPixelsStable {
                        region,
                        poll_ms: resolved_poll_ms(step, self.defaults.default_poll_ms),
                        stability: self.pixel_stability(timeout_ms),
                    },
                ))
            }
            "click_text" => self.prepare_text_step(step, context, true),
            "wait_text" => self.prepare_text_step(step, context, false),
            "paste" | "paste_text" => {
                let text = custom_step_text(step, context);
                if text.is_empty() {
                    bail!("custom workflow paste step missing text");
                }
                Ok(PreparedWorkflowStep::Mechanical(
                    WorkflowOperation::PasteText {
                        text,
                        clipboard_hold_ms: self.defaults.clipboard_hold_ms,
                    },
                ))
            }
            "send_chat" | "reply" => {
                let message = custom_step_message(step, context);
                if message.is_empty() {
                    bail!("custom workflow send_chat step missing message");
                }
                Ok(PreparedWorkflowStep::Capability(
                    WorkflowCapability::SendHall { message },
                ))
            }
            "send_current_chat" => {
                let message = custom_step_message(step, context);
                if message.is_empty() {
                    bail!("custom workflow send_current_chat step missing message");
                }
                Ok(PreparedWorkflowStep::Capability(
                    WorkflowCapability::SendCurrentChat { message },
                ))
            }
            "send_friend_message" | "friend_reply" => {
                let message = custom_step_message(step, context);
                if message.is_empty() {
                    bail!("custom workflow send_friend_message step missing message");
                }
                let target = custom_step_target(step, context);
                if target.is_empty() {
                    bail!("custom workflow send_friend_message step missing target");
                }
                Ok(PreparedWorkflowStep::Capability(
                    WorkflowCapability::SendFriendMessage { target, message },
                ))
            }
            "invite_user" | "invite_current_user" => {
                let target = custom_step_target(step, context);
                if target.is_empty() {
                    bail!("custom workflow invite step missing target");
                }
                Ok(PreparedWorkflowStep::Capability(
                    WorkflowCapability::InviteUser { target },
                ))
            }
            "ensure_primary" => Ok(PreparedWorkflowStep::Mechanical(
                WorkflowOperation::EnsureResidency {
                    target: WorkflowResidency::Primary,
                },
            )),
            "return_primary" => Ok(PreparedWorkflowStep::Mechanical(
                WorkflowOperation::ReturnListenerResidency,
            )),
            "ensure_current_hall" => Ok(PreparedWorkflowStep::Mechanical(
                WorkflowOperation::EnsureResidency {
                    target: WorkflowResidency::SecondaryCurrentHall,
                },
            )),
            other => bail!("unsupported custom workflow step type: {}", other),
        }
    }

    fn prepare_template_step(
        &self,
        step: &CustomWorkflowStep,
        context: &WorkflowContext,
        click: bool,
    ) -> Result<PreparedWorkflowStep> {
        let template = self.resolve_template(step, context)?;
        let region = required_region(step, "custom workflow template step missing region")?;
        let threshold = step.threshold.unwrap_or(self.config.default_threshold);
        let timeout_ms = step.timeout_ms.unwrap_or(self.defaults.default_timeout_ms);
        let poll_ms = resolved_poll_ms(step, self.defaults.default_poll_ms);
        if click {
            Ok(PreparedWorkflowStep::Mechanical(
                WorkflowOperation::ClickTemplate {
                    template,
                    region,
                    threshold,
                    timeout_ms,
                    poll_ms,
                    offset: step.click_offset.unwrap_or(PointConfig::new(0, 0)).into(),
                },
            ))
        } else {
            Ok(PreparedWorkflowStep::Mechanical(
                WorkflowOperation::WaitTemplate {
                    template,
                    region,
                    threshold,
                    timeout_ms,
                    poll_ms,
                },
            ))
        }
    }

    fn prepare_template_absent_step(
        &self,
        step: &CustomWorkflowStep,
        context: &WorkflowContext,
    ) -> Result<PreparedWorkflowStep> {
        let template = self.resolve_template(step, context)?;
        let region = required_region(step, "custom workflow template step missing region")?;
        let poll_ms = resolved_poll_ms(step, self.defaults.default_poll_ms);
        let stable_after_absent = step
            .stable_after_absent
            .unwrap_or(self.config.wait_template_absent_stable_default);
        let stability = stable_after_absent.then(|| {
            self.pixel_stability(
                step.wait_ms
                    .unwrap_or(self.defaults.default_step_wait_ms)
                    .max(poll_ms),
            )
        });
        Ok(PreparedWorkflowStep::Mechanical(
            WorkflowOperation::WaitTemplateAbsent {
                template,
                region,
                threshold: step.threshold.unwrap_or(self.config.default_threshold),
                timeout_ms: step.timeout_ms.unwrap_or(self.defaults.default_timeout_ms),
                poll_ms,
                stability,
            },
        ))
    }

    fn prepare_text_step(
        &self,
        step: &CustomWorkflowStep,
        context: &WorkflowContext,
        click: bool,
    ) -> Result<PreparedWorkflowStep> {
        let expected = custom_step_text(step, context);
        if expected.is_empty() {
            bail!("custom workflow text step missing text");
        }
        let region = required_region(step, "custom workflow text step missing region")?;
        let timeout_ms = step.timeout_ms.unwrap_or(self.defaults.default_timeout_ms);
        let poll_ms = resolved_poll_ms(step, self.defaults.default_poll_ms);
        if click {
            Ok(PreparedWorkflowStep::Mechanical(
                WorkflowOperation::ClickText {
                    expected,
                    region,
                    timeout_ms,
                    poll_ms,
                    offset: step.click_offset.unwrap_or(PointConfig::new(0, 0)).into(),
                },
            ))
        } else {
            Ok(PreparedWorkflowStep::Mechanical(
                WorkflowOperation::WaitText {
                    expected,
                    region,
                    timeout_ms,
                    poll_ms,
                },
            ))
        }
    }

    fn resolve_template(
        &self,
        step: &CustomWorkflowStep,
        context: &WorkflowContext,
    ) -> Result<PathBuf> {
        let name = context.render(step.template.as_deref().unwrap_or("").trim());
        template_path(&self.config, &name)
    }

    fn pixel_stability(&self, timeout_ms: u64) -> WorkflowPixelStability {
        WorkflowPixelStability {
            timeout_ms,
            mean_threshold: self.defaults.stability_mean_threshold,
            changed_ratio_threshold: self.defaults.stability_changed_ratio_threshold,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct WorkflowPlan {
    workflow: String,
    confirmation: Option<WorkflowConfirmation>,
    steps: Vec<WorkflowPlanStep>,
    success_message: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
enum PreparedWorkflowStep {
    Mechanical(WorkflowOperation),
    Capability(WorkflowCapability),
}

#[derive(Clone, Debug, PartialEq)]
enum WorkflowPlanStep {
    ActionPlan(Vec<WorkflowOperation>),
    Capability(WorkflowCapability),
}

fn compile_plan_steps(steps: Vec<PreparedWorkflowStep>) -> Vec<WorkflowPlanStep> {
    let mut compiled = Vec::new();
    let mut operations = Vec::new();
    for step in steps {
        match step {
            PreparedWorkflowStep::Mechanical(operation) => operations.push(operation),
            PreparedWorkflowStep::Capability(capability) => {
                if !operations.is_empty() {
                    compiled.push(WorkflowPlanStep::ActionPlan(std::mem::take(
                        &mut operations,
                    )));
                }
                compiled.push(WorkflowPlanStep::Capability(capability));
            }
        }
    }
    if !operations.is_empty() {
        compiled.push(WorkflowPlanStep::ActionPlan(operations));
    }
    compiled
}

#[derive(Clone, Debug, PartialEq, Eq)]
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
    fn new(invocation: &CustomWorkflowInvocation) -> Self {
        Self {
            workflow: invocation.command.workflow.clone(),
            command: invocation.command.name.clone(),
            args: invocation.command.args.clone(),
            argv: invocation
                .command
                .args
                .split_whitespace()
                .map(str::to_string)
                .collect(),
            username: invocation.username.clone(),
            message_type: invocation.message_type.clone(),
            user_command: invocation.user_command.clone(),
        }
    }

    fn render(&self, text: &str) -> String {
        render_workflow_text(text, self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowConfirmation {
    pub message: String,
    pub message_types: Vec<String>,
    pub timeout_ms: u64,
    pub poll_ms: u64,
}

impl WorkflowConfirmation {
    pub fn accepts_message_type(&self, message_type: &str) -> bool {
        self.message_types.is_empty()
            || self
                .message_types
                .iter()
                .any(|item| item.eq_ignore_ascii_case(message_type))
    }

    pub fn requires_multiple_conversations(&self) -> bool {
        self.accepts_message_type("pink")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkflowConfirmationDecision {
    Confirm,
    Skip,
}

impl From<PointConfig> for WorkflowPoint {
    fn from(value: PointConfig) -> Self {
        Self {
            x: value.x,
            y: value.y,
        }
    }
}

impl From<RectConfig> for WorkflowRect {
    fn from(value: RectConfig) -> Self {
        Self {
            x: value.x,
            y: value.y,
            width: value.width,
            height: value.height,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum WorkflowCapability {
    SendHall { message: String },
    SendCurrentChat { message: String },
    SendFriendMessage { target: String, message: String },
    InviteUser { target: String },
}

fn is_confirmation_message(text: &str) -> bool {
    parse_confirmation(text).is_some()
}

#[cfg(test)]
fn operation_label(operation: &WorkflowOperation) -> &'static str {
    match operation {
        WorkflowOperation::Wait { .. } => "wait",
        WorkflowOperation::PressKey { .. } => "press_key",
        WorkflowOperation::HoldKey { .. } => "hold_key",
        WorkflowOperation::ActivateGame { .. } => "activate_game",
        WorkflowOperation::FocusGame { .. } => "focus_game",
        WorkflowOperation::EnsureResidency { .. } => "ensure_residency",
        WorkflowOperation::ReturnListenerResidency => "return_listener_residency",
        WorkflowOperation::ClickPoint { .. } => "click",
        WorkflowOperation::ClickMouseButton { .. } => "mouse_button",
        WorkflowOperation::WaitTemplate { .. } => "wait_template",
        WorkflowOperation::ClickTemplate { .. } => "click_template",
        WorkflowOperation::WaitTemplateAbsent { .. } => "wait_template_absent",
        WorkflowOperation::WaitPixelsStable { .. } => "wait_pixels_stable",
        WorkflowOperation::WaitText { .. } => "wait_text",
        WorkflowOperation::ClickText { .. } => "click_text",
        WorkflowOperation::PasteText { .. } => "paste_text",
    }
}

fn capability_label(capability: &WorkflowCapability) -> &'static str {
    match capability {
        WorkflowCapability::SendHall { .. } => "send_chat",
        WorkflowCapability::SendCurrentChat { .. } => "send_current_chat",
        WorkflowCapability::SendFriendMessage { .. } => "send_friend_message",
        WorkflowCapability::InviteUser { .. } => "invite_user",
    }
}

fn find_workflow<'a>(
    config: &'a CustomWorkflowConfig,
    name: &str,
) -> Option<&'a CustomWorkflowDefinition> {
    let target = normalize_name(name);
    config
        .workflows
        .iter()
        .find(|workflow| workflow.enabled && workflow_matches_name(workflow, &target))
}

fn template_path(config: &CustomWorkflowConfig, template: &str) -> Result<PathBuf> {
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

fn parse_confirmation(text: &str) -> Option<WorkflowConfirmationDecision> {
    let raw = text.trim();
    let command_text = if let Some(index) = raw.find(['：', ':', ']', '】']) {
        let separator_len = raw[index..].chars().next()?.len_utf8();
        &raw[index + separator_len..]
    } else {
        raw
    }
    .trim_start_matches(['：', ':', ' ', '\t', ']', '】']);
    if command_text
        .strip_prefix("@确认")
        .is_some_and(|rest| decision_boundary(rest.chars().next()))
    {
        Some(WorkflowConfirmationDecision::Confirm)
    } else if command_text
        .strip_prefix("@跳过")
        .is_some_and(|rest| decision_boundary(rest.chars().next()))
    {
        Some(WorkflowConfirmationDecision::Skip)
    } else {
        None
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

fn required_region(step: &CustomWorkflowStep, error: &str) -> Result<WorkflowRect> {
    step.region
        .map(Into::into)
        .ok_or_else(|| anyhow!(error.to_string()))
}

fn resolved_poll_ms(step: &CustomWorkflowStep, default_poll_ms: u64) -> u64 {
    step.poll_ms.unwrap_or(default_poll_ms).max(MIN_POLL_MS)
}

fn custom_step_text(step: &CustomWorkflowStep, context: &WorkflowContext) -> String {
    let text = step.text.as_deref().unwrap_or("").trim();
    let value = if text.is_empty() {
        step.message.as_deref().unwrap_or("").trim()
    } else {
        text
    };
    context.render(value)
}

fn custom_step_message(step: &CustomWorkflowStep, context: &WorkflowContext) -> String {
    let message = step.message.as_deref().unwrap_or("").trim();
    let value = if message.is_empty() {
        step.text.as_deref().unwrap_or("").trim()
    } else {
        message
    };
    context.render(value)
}

fn custom_step_target(step: &CustomWorkflowStep, context: &WorkflowContext) -> String {
    let target = step.target.as_deref().unwrap_or("").trim();
    if target.is_empty() {
        context.username.trim().to_string()
    } else {
        context.render(target)
    }
}

fn custom_hold_key_seconds(
    step: &CustomWorkflowStep,
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

fn step_consumes_wait(step: &CustomWorkflowStep, stable_absent_default: bool) -> bool {
    match step.step_type.trim() {
        "sleep" | "wait" | "hold_key" => true,
        "wait_template_absent" => step.stable_after_absent.unwrap_or(stable_absent_default),
        "wait_stable" | "wait_pixels_stable" => true,
        _ => false,
    }
}

fn parse_mouse_button(value: &str) -> Result<WorkflowMouseButton> {
    match value.trim().to_ascii_lowercase().as_str() {
        "left" => Ok(WorkflowMouseButton::Left),
        "middle" => Ok(WorkflowMouseButton::Middle),
        "right" => Ok(WorkflowMouseButton::Right),
        "" => bail!("custom workflow mouse_button step missing button"),
        value => bail!("unsupported custom workflow mouse button: {value}"),
    }
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
            let Some(rest) = strip_ascii_case_prefix(command_text, command) else {
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

fn strip_ascii_case_prefix<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    if text.len() < prefix.len() || !text.is_char_boundary(prefix.len()) {
        return None;
    }
    let candidate = &text[..prefix.len()];
    candidate
        .eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
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

fn command_boundary(ch: Option<char>) -> bool {
    decision_boundary(ch)
}

fn normalize_name(text: &str) -> String {
    text.trim().to_ascii_lowercase()
}

fn identity_text(text: &str) -> String {
    let normalized = normalize_comparison_text(text);
    if normalized.is_empty() {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    } else {
        normalized
    }
}

fn joined_command(command: &str, args: &str) -> String {
    if args.is_empty() {
        command.to_string()
    } else {
        format!("{command} {args}")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;

    use super::*;
    use crate::config::AppConfig;
    use crate::features::command::CommandObservation;

    fn envelope(username: &str, message_type: &str, command: &str) -> CommandEnvelope {
        CommandEnvelope::new(
            command,
            username,
            message_type,
            command,
            CommandObservation::default(),
        )
        .expect("command envelope")
    }

    struct RecordingPort {
        events: Vec<String>,
        confirmation: FreshMessageOutcome,
    }

    impl RecordingPort {
        fn new(confirmation: FreshMessageOutcome) -> Self {
            Self {
                events: Vec::new(),
                confirmation,
            }
        }
    }

    impl CustomWorkflowExecutionPort for RecordingPort {
        fn send_hall(&mut self, message: &str) -> Result<()> {
            self.events.push(format!("send:{message}"));
            Ok(())
        }

        fn wait_for_fresh_message(
            &mut self,
            confirmation: &WorkflowConfirmation,
            accepts_text: fn(&str) -> bool,
        ) -> Result<FreshMessageOutcome> {
            self.events.push(format!(
                "wait:{}:{}",
                confirmation.timeout_ms, confirmation.poll_ms
            ));
            if let FreshMessageOutcome::Message(message) = &self.confirmation {
                assert!(accepts_text(message));
            }
            Ok(self.confirmation.clone())
        }

        fn execute_action_plan(
            &mut self,
            workflow: &str,
            operations: Vec<WorkflowOperation>,
        ) -> Result<()> {
            self.events.push(format!(
                "action-plan:{workflow}:{}",
                operations
                    .iter()
                    .map(operation_label)
                    .collect::<Vec<_>>()
                    .join(",")
            ));
            Ok(())
        }

        fn execute_capability(
            &mut self,
            workflow: &str,
            capability: WorkflowCapability,
        ) -> Result<()> {
            self.events.push(format!(
                "capability:{workflow}:{}",
                capability_label(&capability)
            ));
            Ok(())
        }
    }

    fn defaults() -> WorkflowDefaults {
        WorkflowDefaults {
            default_timeout_ms: 4_000,
            default_poll_ms: 20,
            default_step_wait_ms: 300,
            decision_timeout_ms: 20_000,
            decision_poll_ms: 2_000,
            after_activate_ms: 80,
            clipboard_hold_ms: 120,
            stability_mean_threshold: 1.5,
            stability_changed_ratio_threshold: 0.02,
        }
    }

    fn workflow() -> CustomWorkflowDefinition {
        CustomWorkflowDefinition {
            enabled: true,
            name: "example".to_string(),
            commands: vec!["测试流程".to_string()],
            allow_args: true,
            message_types: vec!["blue".to_string()],
            confirm_before_run: false,
            confirm_message: String::new(),
            confirm_message_types: vec!["blue".to_string()],
            confirm_timeout_ms: None,
            confirm_poll_ms: None,
            steps: vec![step("key")],
            success_message: String::new(),
        }
    }

    fn config(workflow: CustomWorkflowDefinition) -> CustomWorkflowConfig {
        CustomWorkflowConfig {
            enabled: true,
            default_threshold: 0.9,
            wait_template_absent_stable_default: true,
            max_hold_key_seconds: 10,
            templates: HashMap::from([("button".to_string(), PathBuf::from("assets/button.png"))]),
            workflows: vec![workflow],
        }
    }

    #[test]
    fn configuration_rejects_unsupported_steps_before_runtime_startup() {
        let mut value = workflow();
        value.steps = vec![step("unsupported")];

        let error = config(value).validate().unwrap_err();

        assert!(
            error
                .to_string()
                .contains("unsupported custom workflow step type: unsupported")
        );
    }

    #[test]
    fn configuration_rejects_missing_step_fields_before_runtime_startup() {
        for (kind, expected) in [
            ("key", "step key is empty"),
            ("click", "missing point"),
            ("mouse_button", "missing button"),
            ("wait_template", "template is empty"),
            ("wait_text", "missing text"),
            ("paste", "missing text"),
            ("send_chat", "missing message"),
            ("send_current_chat", "missing message"),
            ("send_friend_message", "missing message"),
        ] {
            let mut value = workflow();
            let mut invalid = step(kind);
            invalid.key = None;
            value.steps = vec![invalid];

            let error = config(value).validate().unwrap_err();

            assert!(
                error.to_string().contains(expected),
                "kind={kind} error={error:#}"
            );
        }
    }

    fn step(kind: &str) -> CustomWorkflowStep {
        CustomWorkflowStep {
            step_type: kind.to_string(),
            template: None,
            region: None,
            point: None,
            click_offset: None,
            key: Some("F".to_string()),
            button: None,
            target: None,
            text: None,
            message: None,
            threshold: None,
            timeout_ms: None,
            poll_ms: None,
            wait_ms: None,
            hold_seconds_arg: None,
            stable_after_absent: None,
        }
    }

    fn invocation(args: &str) -> CustomWorkflowInvocation {
        CustomWorkflowInvocation {
            command: CustomWorkflowCommand {
                name: "测试流程".to_string(),
                workflow: "example".to_string(),
                args: args.to_string(),
            },
            username: "用户".to_string(),
            message_type: "blue".to_string(),
            user_command: joined_command("@测试流程", args),
        }
    }

    fn prepared_step(kind: &str) -> PreparedWorkflowStep {
        let mut value = step(kind);
        match kind {
            "sleep" | "wait" => value.wait_ms = Some(10),
            "click" => value.point = Some(PointConfig::new(1, 2)),
            "mouse_button" => value.button = Some("middle".to_string()),
            "click_template" | "wait_template" | "wait_template_absent" => {
                value.template = Some("button".to_string());
                value.region = Some(RectConfig {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                });
            }
            "wait_stable" | "wait_pixels_stable" => {
                value.region = Some(RectConfig {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                });
            }
            "click_text" | "wait_text" | "paste" | "paste_text" => {
                value.text = Some("文本".to_string());
                if matches!(kind, "click_text" | "wait_text") {
                    value.region = Some(RectConfig {
                        x: 1,
                        y: 2,
                        width: 3,
                        height: 4,
                    });
                }
            }
            "send_chat" | "reply" | "send_current_chat" => {
                value.message = Some("消息".to_string());
            }
            "send_friend_message" | "friend_reply" => {
                value.message = Some("消息".to_string());
                value.target = Some("好友".to_string());
            }
            "invite_user" | "invite_current_user" => {
                value.target = Some("好友".to_string());
            }
            _ => {}
        }
        let service = CustomWorkflowService::new(config(workflow()), defaults());
        service
            .prepare_step(&value, &WorkflowContext::new(&invocation("3")))
            .unwrap()
    }

    fn only_action_plan(plan: &WorkflowPlan) -> &[WorkflowOperation] {
        match plan.steps.as_slice() {
            [WorkflowPlanStep::ActionPlan(operations)] => operations,
            steps => panic!("expected one action plan, got {steps:?}"),
        }
    }

    #[test]
    fn execute_owns_confirmation_operations_and_success_message() {
        let mut value = workflow();
        value.confirm_before_run = true;
        value.confirm_message = "确认 {{username}}".to_string();
        value.success_message = "完成 {{arg1}}".to_string();
        let service = CustomWorkflowService::new(config(value), defaults());
        let mut port = RecordingPort::new(FreshMessageOutcome::Message("用户：@确认".to_string()));

        let completion = service.execute(&invocation("3"), &mut port).unwrap();

        assert_eq!(completion, WorkflowCompletion::Completed);
        assert_eq!(
            port.events,
            [
                "send:确认 用户",
                "wait:20000:2000",
                "action-plan:example:press_key,wait",
                "send:完成 3",
            ]
        );
    }

    #[test]
    fn execute_timeout_cancels_without_running_any_operation() {
        let mut value = workflow();
        value.confirm_before_run = true;
        let service = CustomWorkflowService::new(config(value), defaults());
        let mut port = RecordingPort::new(FreshMessageOutcome::Timeout);

        let completion = service.execute(&invocation(""), &mut port).unwrap();

        assert_eq!(completion, WorkflowCompletion::Cancelled);
        assert_eq!(
            port.events,
            [
                "send:用户 请求执行 测试流程,@确认@跳过",
                "wait:20000:2000",
                "send:自定义流程确认超时,已取消",
            ]
        );
    }

    #[test]
    fn execute_skip_and_stop_cancel_silently() {
        let mut value = workflow();
        value.confirm_before_run = true;
        let service = CustomWorkflowService::new(config(value), defaults());

        for outcome in [
            FreshMessageOutcome::Message("用户：@跳过".to_string()),
            FreshMessageOutcome::Stopped,
        ] {
            let mut port = RecordingPort::new(outcome);
            assert_eq!(
                service.execute(&invocation(""), &mut port).unwrap(),
                WorkflowCompletion::Cancelled
            );
            assert_eq!(port.events.len(), 2);
            assert!(port.events[0].starts_with("send:"));
            assert!(port.events[1].starts_with("wait:"));
        }
    }

    #[test]
    fn execute_validates_the_complete_plan_before_any_port_call() {
        let mut value = workflow();
        value.confirm_before_run = true;
        value.steps.push(step("unsupported"));
        let service = CustomWorkflowService::new(config(value), defaults());
        let mut port = RecordingPort::new(FreshMessageOutcome::Timeout);

        assert!(service.execute(&invocation(""), &mut port).is_err());
        assert!(port.events.is_empty());
    }

    #[test]
    fn execute_groups_mechanical_steps_and_splits_them_at_typed_capabilities() {
        let mut value = workflow();
        let mut click = step("click");
        click.point = Some(PointConfig::new(1, 2));
        let mut friend = step("send_friend_message");
        friend.target = Some("好友".to_string());
        friend.message = Some("消息".to_string());
        let mut paste = step("paste");
        paste.text = Some("文本".to_string());
        let mut invite = step("invite_user");
        invite.target = Some("好友".to_string());
        value.steps = vec![step("key"), click, friend, paste, invite];
        let service = CustomWorkflowService::new(config(value), defaults());
        let mut port = RecordingPort::new(FreshMessageOutcome::Stopped);

        assert_eq!(
            service.execute(&invocation(""), &mut port).unwrap(),
            WorkflowCompletion::Completed
        );
        assert_eq!(
            port.events,
            [
                "action-plan:example:press_key,wait,click,wait",
                "capability:example:send_friend_message",
                "action-plan:example:wait,paste_text,wait",
                "capability:example:invite_user",
                "action-plan:example:wait",
            ]
        );
    }

    #[test]
    fn parses_chat_commands_with_all_supported_argument_layouts() {
        let service = CustomWorkflowService::new(config(workflow()), defaults());
        for (command, expected) in [
            ("@测试流程 123 abc", "123 abc"),
            ("@测试流程123 abc", "123 abc"),
            ("@测试流程：123 abc", "123 abc"),
        ] {
            let parsed = service
                .parse_chat(&envelope("用户", "blue", command))
                .unwrap();
            assert_eq!(parsed.command.args, expected);
            assert_eq!(parsed.raw, "测试流程 123 abc");
        }
    }

    #[test]
    fn chat_parser_respects_enablement_source_and_no_argument_boundary() {
        let mut value = workflow();
        value.allow_args = false;
        let service = CustomWorkflowService::new(config(value.clone()), defaults());
        assert!(
            service
                .parse_chat(&envelope("用户", "blue", "@测试流程"))
                .is_some()
        );
        assert!(
            service
                .parse_chat(&envelope("用户", "blue", "@测试流程参数"))
                .is_none()
        );
        assert!(
            service
                .parse_chat(&envelope("用户", "pink", "@测试流程"))
                .is_none()
        );

        value.message_types.clear();
        let mut disabled = config(value);
        disabled.enabled = false;
        let service = CustomWorkflowService::new(disabled, defaults());
        assert!(
            service
                .parse_chat(&envelope("用户", "blue", "@测试流程"))
                .is_none()
        );
    }

    #[test]
    fn remote_prepare_uses_first_command_and_bypasses_chat_source_filter() {
        let mut value = workflow();
        value.commands = vec!["@入口".to_string(), "别名".to_string()];
        value.message_types = vec!["pink".to_string()];
        let service = CustomWorkflowService::new(config(value), defaults());

        let prepared = service.prepare_remote("example", "5").unwrap();

        assert_eq!(prepared.raw, "入口 5");
        assert_eq!(prepared.matched, "入口");
        assert_eq!(prepared.command.workflow, "example");
    }

    #[test]
    fn workflow_without_commands_is_remote_only() {
        let mut value = workflow();
        value.name = "鼠标中键".to_string();
        value.commands.clear();
        value.allow_args = false;
        value.message_types = vec!["pink".to_string()];
        let service = CustomWorkflowService::new(config(value), defaults());
        let chat = envelope("用户", "pink", "@鼠标中键");

        assert!(!service.claims_chat(&chat));
        assert!(service.parse_chat(&chat).is_none());

        let prepared = service.prepare_remote("鼠标中键", "").unwrap();
        assert_eq!(prepared.raw, "鼠标中键");
        assert_eq!(prepared.matched, "鼠标中键");
        assert_eq!(prepared.command.workflow, "鼠标中键");
    }

    #[test]
    fn default_control_commands_resolve_shared_prefixes_and_duration_arguments() {
        let app = AppConfig::load(Path::new("config.yaml")).expect("default config");
        let service = CustomWorkflowService::new(app.custom_workflows, defaults());

        for (text, workflow, args) in [
            ("@C", "press-control", ""),
            ("@CW 3", "control-hold-w", "3"),
            ("@CS 2", "control-hold-s", "2"),
            ("@CA", "control-hold-a", ""),
            ("@CD 10", "control-hold-d", "10"),
        ] {
            let chat = envelope("用户", "pink", text);
            assert!(
                service.claims_chat(&chat),
                "command was not claimed: {text}"
            );
            let parsed = service
                .parse_chat(&chat)
                .unwrap_or_else(|| panic!("command was not parsed: {text}"));
            assert_eq!(parsed.command.workflow, workflow);
            assert_eq!(parsed.command.args, args);
        }

        let middle = envelope("用户", "pink", "@鼠标中键");
        assert!(!service.claims_chat(&middle));
        assert!(service.parse_chat(&middle).is_none());
        assert!(service.prepare_remote("鼠标中键", "").is_ok());
    }

    #[test]
    fn list_keeps_enabled_definitions_even_when_global_switch_is_off() {
        let mut first = workflow();
        first.name.clear();
        let mut second = workflow();
        second.name = "disabled".to_string();
        second.enabled = false;
        let mut config = config(first);
        config.enabled = false;
        config.workflows.push(second);
        let service = CustomWorkflowService::new(config, defaults());

        assert_eq!(
            service.list(),
            vec![CustomWorkflowSummary {
                name: "测试流程".to_string(),
                commands: vec!["测试流程".to_string()],
                allow_args: true,
                confirm_before_run: false,
            }]
        );
    }

    #[test]
    fn lock_identity_matches_previous_normalized_semantics() {
        let first = CustomWorkflowCommand {
            name: "ignored".to_string(),
            workflow: " Test-Flow ".to_string(),
            args: "A：B".to_string(),
        };
        let second = CustomWorkflowCommand {
            name: "different".to_string(),
            workflow: "test flow".to_string(),
            args: "a b".to_string(),
        };

        assert!(first.same_request(&second));
        assert_eq!(first.lock_key(), "custom_workflow:testflow:ab");
    }

    #[test]
    fn context_renders_all_variables_and_preserves_unknown_values() {
        let context = WorkflowContext::new(&invocation("123 abc"));
        assert_eq!(
            context.render(
                "{{workflow}}|{{command}}|{{args}}|{{arg1}}|{{arg2}}|{{username}}|{{message_type}}|{{user_command}}|{{unknown}}"
            ),
            "example|测试流程|123 abc|123|abc|用户|blue|@测试流程 123 abc|{{unknown}}"
        );
    }

    #[test]
    fn prepare_builds_confirmation_and_success_message_before_execution() {
        let mut value = workflow();
        value.confirm_before_run = true;
        value.confirm_message = "{{username}} 确认 {{arg1}}".to_string();
        value.confirm_message_types = vec!["PINK".to_string()];
        value.confirm_poll_ms = Some(1);
        value.success_message = "完成 {{args}}".to_string();
        let service = CustomWorkflowService::new(config(value), defaults());

        let plan = service.prepare(&invocation("3")).unwrap();

        assert_eq!(plan.success_message.as_deref(), Some("完成 3"));
        let confirmation = plan.confirmation.unwrap();
        assert_eq!(confirmation.message, "用户 确认 3");
        assert_eq!(confirmation.poll_ms, MIN_POLL_MS);
        assert!(confirmation.requires_multiple_conversations());
    }

    #[test]
    fn prepare_rejects_any_later_invalid_step_before_returning_a_plan() {
        let mut value = workflow();
        value.steps = vec![step("key"), step("unsupported")];
        let service = CustomWorkflowService::new(config(value), defaults());

        let error = service.prepare(&invocation("")).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("unsupported custom workflow step type: unsupported")
        );
    }

    #[test]
    fn prepare_rejects_missing_fields_for_each_effectful_step() {
        for (kind, expected) in [
            ("key", "step key is empty"),
            ("click", "missing point"),
            ("mouse_button", "missing button"),
            ("wait_template", "template is empty"),
            ("wait_text", "missing text"),
            ("paste", "missing text"),
            ("send_chat", "missing message"),
            ("send_current_chat", "missing message"),
            ("send_friend_message", "missing message"),
        ] {
            let mut value = workflow();
            let mut invalid = step(kind);
            invalid.key = None;
            value.steps = vec![invalid];
            let error = CustomWorkflowService::new(config(value), defaults())
                .prepare(&invocation(""))
                .unwrap_err();
            assert!(
                error.to_string().contains(expected),
                "kind={kind} error={error:#}"
            );
        }
    }

    #[test]
    fn prepare_supports_every_current_step_alias() {
        for (left, right) in [
            ("sleep", "wait"),
            ("key", "press_key"),
            ("wait_stable", "wait_pixels_stable"),
            ("paste", "paste_text"),
            ("send_chat", "reply"),
            ("send_friend_message", "friend_reply"),
            ("invite_user", "invite_current_user"),
        ] {
            assert_eq!(prepared_step(left), prepared_step(right));
        }

        assert_eq!(
            prepared_step("return_primary"),
            PreparedWorkflowStep::Mechanical(WorkflowOperation::ReturnListenerResidency)
        );
        assert_eq!(
            prepared_step("ensure_primary"),
            PreparedWorkflowStep::Mechanical(WorkflowOperation::EnsureResidency {
                target: WorkflowResidency::Primary,
            })
        );

        for kind in [
            "hold_key",
            "activate_game",
            "focus_game",
            "click",
            "mouse_button",
            "click_template",
            "wait_template",
            "wait_template_absent",
            "click_text",
            "wait_text",
            "send_current_chat",
            "ensure_current_hall",
        ] {
            let _ = prepared_step(kind);
        }
    }

    #[test]
    fn mouse_button_accepts_only_named_primary_buttons() {
        for (configured, expected) in [
            ("left", WorkflowMouseButton::Left),
            ("MIDDLE", WorkflowMouseButton::Middle),
            ("right", WorkflowMouseButton::Right),
        ] {
            let mut value = step("mouse_button");
            value.button = Some(configured.to_string());
            assert_eq!(
                CustomWorkflowService::new(config(workflow()), defaults())
                    .prepare_step(&value, &WorkflowContext::new(&invocation("")))
                    .unwrap(),
                PreparedWorkflowStep::Mechanical(WorkflowOperation::ClickMouseButton {
                    button: expected,
                })
            );
        }

        let mut value = workflow();
        let mut invalid = step("mouse_button");
        invalid.button = Some("back".to_string());
        value.steps = vec![invalid];
        let error = config(value).validate().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported custom workflow mouse button: back")
        );
    }

    #[test]
    fn prepare_resolves_template_defaults_and_explicit_post_wait() {
        let mut value = workflow();
        let mut template = step("click_template");
        template.template = Some("button".to_string());
        template.region = Some(RectConfig {
            x: 10,
            y: 20,
            width: 30,
            height: 40,
        });
        template.wait_ms = Some(75);
        value.steps = vec![template];
        let plan = CustomWorkflowService::new(config(value), defaults())
            .prepare(&invocation(""))
            .unwrap();

        let operations = only_action_plan(&plan);
        assert_eq!(operations.len(), 2);
        assert!(matches!(
            &operations[0],
            WorkflowOperation::ClickTemplate {
                template,
                threshold,
                timeout_ms: 4_000,
                poll_ms: 50,
                ..
            } if template == &PathBuf::from("assets/button.png") && *threshold == 0.9
        ));
        assert_eq!(operations[1], WorkflowOperation::Wait { duration_ms: 75 });
    }

    #[test]
    fn absent_template_stability_consumes_wait_but_disabled_stability_adds_it() {
        let make_plan = |stable_after_absent| {
            let mut value = workflow();
            let mut absent = step("wait_template_absent");
            absent.template = Some("button".to_string());
            absent.region = Some(RectConfig {
                x: 1,
                y: 2,
                width: 3,
                height: 4,
            });
            absent.wait_ms = Some(80);
            absent.stable_after_absent = Some(stable_after_absent);
            value.steps = vec![absent];
            CustomWorkflowService::new(config(value), defaults())
                .prepare(&invocation(""))
                .unwrap()
        };

        let stable = make_plan(true);
        let stable_operations = only_action_plan(&stable);
        assert_eq!(stable_operations.len(), 1);
        assert!(matches!(
            &stable_operations[0],
            WorkflowOperation::WaitTemplateAbsent {
                stability: Some(WorkflowPixelStability { timeout_ms: 80, .. }),
                ..
            }
        ));

        let not_stable = make_plan(false);
        let not_stable_operations = only_action_plan(&not_stable);
        assert_eq!(not_stable_operations.len(), 2);
        assert!(matches!(
            &not_stable_operations[0],
            WorkflowOperation::WaitTemplateAbsent {
                stability: None,
                ..
            }
        ));
        assert_eq!(
            not_stable_operations[1],
            WorkflowOperation::Wait { duration_ms: 80 }
        );
    }

    #[test]
    fn hold_key_uses_selected_argument_and_never_adds_post_wait() {
        let mut value = workflow();
        let mut hold = step("hold_key");
        hold.key = Some("{{command}}".to_string());
        hold.hold_seconds_arg = Some(2);
        hold.wait_ms = Some(999);
        value.steps = vec![hold];
        let plan = CustomWorkflowService::new(config(value), defaults())
            .prepare(&invocation("unused 7"))
            .unwrap();

        assert_eq!(
            only_action_plan(&plan),
            [WorkflowOperation::HoldKey {
                key: "测试流程".to_string(),
                duration_seconds: 7,
            }]
        );
    }

    #[test]
    fn friend_and_invite_targets_default_to_triggering_user() {
        let mut value = workflow();
        let mut friend = step("send_friend_message");
        friend.message = Some("你好 {{username}}".to_string());
        let invite = step("invite_user");
        value.steps = vec![friend, invite];
        let plan = CustomWorkflowService::new(config(value), defaults())
            .prepare(&invocation(""))
            .unwrap();

        assert!(plan.steps.iter().any(|step| matches!(
            step,
            WorkflowPlanStep::Capability(WorkflowCapability::SendFriendMessage {
                target,
                message,
            }) if target == "用户" && message == "你好 用户"
        )));
        assert!(plan.steps.iter().any(|step| matches!(
            step,
            WorkflowPlanStep::Capability(WorkflowCapability::InviteUser { target })
                if target == "用户"
        )));
    }

    #[test]
    fn confirmation_parser_requires_a_complete_command_boundary() {
        assert_eq!(
            parse_confirmation("用户：@确认"),
            Some(WorkflowConfirmationDecision::Confirm)
        );
        assert_eq!(
            parse_confirmation("[用户]：@跳过！"),
            Some(WorkflowConfirmationDecision::Skip)
        );
        assert_eq!(parse_confirmation("用户：@确认其他"), None);
    }
}
