use std::sync::atomic::Ordering as AtomicOrdering;
use std::thread::sleep;
use std::time::{Duration, Instant};

use super::{ApplicationRuntime, ChatDecisionScope};
use crate::features::command::RoutedCommand;
use crate::features::custom_workflow::{
    CustomWorkflowCommand, CustomWorkflowExecutionPort, CustomWorkflowInvocation,
    FreshMessageOutcome, WorkflowCapability, WorkflowConfirmation, WorkflowOperation,
};
use crate::features::friend_delivery::{
    FriendBatchFailure, FriendBatchFailureKind, FriendBatchOutcome, FriendMessage,
};
use crate::features::invite::{
    InviteDecision, InviteExecutionPort, InviteRequest, InviteStart,
    InviteUiOutcome as BusinessInviteUiOutcome,
};
use crate::privacy::redacted_chat_text;
use crate::ui::chat_output::fit_chat_message;
use crate::ui::routines::{
    CustomActionPlan, ExecuteInvite, FriendDelivery, FriendDeliveryMessageStatus, InviteEffect,
    InviteNotificationOutcome, SendFriendDeliveries, UiResidencyTarget,
};
use anyhow::{Result, anyhow};

enum ChatDecisionWait<T> {
    Found(T),
    Timeout,
    Stopped,
}

impl ApplicationRuntime {
    pub(super) fn execute_custom_workflow(
        &mut self,
        command: &CustomWorkflowCommand,
        parsed: &RoutedCommand,
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
            sleep(Duration::from_millis(reader.poll_interval_ms(poll_ms)));
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

    fn notify_friend_invite_decision(&self, username: &str, message: &str) -> bool {
        match self.send_friend_message(username, message) {
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

    fn on_entered_new_hall(&self) -> Result<()> {
        log::info!("已进入新大厅，重置命令识别状态");
        self.abort_entertainment_for_context_loss("邀请流程已进入新大厅");
        self.business.set_commands_enabled(true)?;
        self.screen_lock_primed.store(false, AtomicOrdering::SeqCst);
        self.reset_locks_requested
            .store(true, AtomicOrdering::SeqCst);
        self.clear_hall_countdown_cache_for_new_visual_session("已进入新大厅")?;
        Ok(())
    }

    pub(super) fn send_friend_message(&self, username: &str, message: &str) -> Result<bool> {
        log::info!("好友发言: {} -> {}", username, redacted_chat_text(message));
        self.send_friend_delivery_routine(username, message)
    }

    pub(super) fn send_unique_friend_message(&self, username: &str, message: &str) -> Result<bool> {
        self.send_friend_message(username, message)
    }

    pub(super) fn send_stable_unique_friend_message(
        &self,
        username: &str,
        message: &str,
    ) -> Result<bool> {
        self.send_friend_message(username, message)
    }

    fn send_friend_delivery_routine(&self, username: &str, message: &str) -> Result<bool> {
        let outcome = self.send_friend_delivery_batch(&[FriendMessage::new(
            username,
            fit_chat_message(message),
        )])?;
        match outcome {
            FriendBatchOutcome::Complete => Ok(true),
            FriendBatchOutcome::Failed { failure, .. }
                if failure.kind() == FriendBatchFailureKind::ConfirmedUnsent =>
            {
                Ok(false)
            }
            FriendBatchOutcome::Failed { failure, .. } => Err(anyhow!(
                "好友消息发送结果未知，禁止自动重试: {}",
                failure.reason()
            )),
        }
    }

    pub(super) fn send_friend_delivery_batch(
        &self,
        messages: &[FriendMessage],
    ) -> Result<FriendBatchOutcome> {
        let residency = match self.active_ui_residency()? {
            super::UiResidency::Primary => UiResidencyTarget::Primary,
            super::UiResidency::SecondaryCurrentHall => UiResidencyTarget::SecondaryCurrentHall,
        };
        let mut grouped = Vec::<(String, Vec<String>)>::new();
        for message in messages {
            if let Some((recipient, items)) = grouped.last_mut()
                && recipient == message.recipient()
            {
                items.push(message.message().to_string());
            } else {
                grouped.push((
                    message.recipient().to_string(),
                    vec![message.message().to_string()],
                ));
            }
        }
        let request = SendFriendDeliveries::new(
            grouped
                .into_iter()
                .map(|(recipient, messages)| FriendDelivery::new(recipient, messages))
                .collect(),
            residency,
        );
        let operation = match self.friend_delivery_ui.submit(request.clone()) {
            Ok(operation) => operation,
            Err(error) => {
                return Ok(FriendBatchOutcome::Failed {
                    retryable: messages.to_vec(),
                    failure: FriendBatchFailure::new(
                        FriendBatchFailureKind::ConfirmedUnsent,
                        format!("好友投递未进入 UI runtime: {error}"),
                    ),
                });
            }
        };
        let outcome = match operation.wait() {
            Ok(outcome) => outcome,
            Err(error) => {
                return Ok(FriendBatchOutcome::Failed {
                    retryable: Vec::new(),
                    failure: FriendBatchFailure::new(
                        FriendBatchFailureKind::ResultUnknown,
                        format!("等待好友投递结果失败: {error}"),
                    ),
                });
            }
        };
        let messages_complete = outcome.deliveries().iter().all(|delivery| {
            delivery
                .message_statuses()
                .iter()
                .all(|status| *status == FriendDeliveryMessageStatus::Sent)
        });
        if messages_complete {
            if !outcome.is_complete() {
                log::error!(
                    "好友消息已全部发送，但监听驻留恢复失败: {:?}",
                    outcome.residency()
                );
                return Ok(FriendBatchOutcome::Failed {
                    retryable: Vec::new(),
                    failure: FriendBatchFailure::new(
                        FriendBatchFailureKind::ResultUnknown,
                        format!(
                            "好友消息已发送，但监听驻留恢复失败: {:?}",
                            outcome.residency()
                        ),
                    ),
                });
            }
            return Ok(FriendBatchOutcome::Complete);
        }

        let retryable = outcome
            .safe_retry_request(&request)
            .map(|request| {
                request
                    .deliveries()
                    .iter()
                    .flat_map(|delivery| {
                        delivery.messages().iter().map(|message| {
                            FriendMessage::new(delivery.recipient(), message.clone())
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let result_unknown = outcome.deliveries().iter().any(|delivery| {
            delivery
                .message_statuses()
                .contains(&FriendDeliveryMessageStatus::ResultUnknown)
        });
        let reason = outcome
            .deliveries()
            .iter()
            .find_map(|delivery| delivery.failure())
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("好友投递未完成: {:?}", outcome.residency()));
        Ok(FriendBatchOutcome::Failed {
            retryable,
            failure: FriendBatchFailure::new(
                if result_unknown {
                    FriendBatchFailureKind::ResultUnknown
                } else {
                    FriendBatchFailureKind::ConfirmedUnsent
                },
                reason,
            ),
        })
    }
}

impl CustomWorkflowExecutionPort for ApplicationRuntime {
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

    fn execute_action_plan(
        &mut self,
        workflow: &str,
        operations: Vec<WorkflowOperation>,
    ) -> Result<()> {
        let operations =
            super::resolve_workflow_listener_residency(operations, self.active_ui_residency()?);
        let expected = operations.len();
        if expected == 0 {
            return Err(anyhow!("custom action plan must not be empty"));
        }
        let outcome = self
            .custom_action_ui
            .submit(CustomActionPlan::new(workflow, operations))
            .map_err(|error| anyhow!("自定义动作计划未进入 UI runtime: {error}"))?
            .wait()
            .map_err(|error| anyhow!("等待自定义动作计划结果失败: {error}"))?;
        if outcome.is_complete() && outcome.completed() == expected {
            return Ok(());
        }
        let reason = outcome
            .failure()
            .map(ToString::to_string)
            .unwrap_or_else(|| {
                "custom action plan ended before all operations completed".to_string()
            });
        Err(anyhow!(
            "自定义动作计划未完成: workflow={} completed={}/{} reason={}",
            workflow,
            outcome.completed(),
            expected,
            reason
        ))
    }

    fn execute_capability(
        &mut self,
        _workflow: &str,
        capability: WorkflowCapability,
    ) -> Result<()> {
        match capability {
            WorkflowCapability::SendHall { message } => self.reply(&message),
            WorkflowCapability::SendCurrentChat { message } => {
                self.chat_output.send_current_chat(&message)
            }
            WorkflowCapability::SendFriendMessage { target, message } => {
                if self.send_friend_message(&target, &message)? {
                    Ok(())
                } else {
                    Err(anyhow!(
                        "custom workflow send_friend_message target not found: {}",
                        target
                    ))
                }
            }
            WorkflowCapability::InviteUser { target } => {
                let InviteStart::Ready(execution) = self
                    .business
                    .begin_invite(InviteRequest::new(target, None, None))?
                else {
                    unreachable!("unsequenced custom workflow invites cannot be duplicates")
                };
                execution.run(self).map(|_| ())
            }
        }
    }
}

impl InviteExecutionPort for ApplicationRuntime {
    fn is_public_hall(&self) -> Result<bool> {
        self.check_public_hall()
    }

    fn notify_friend(&self, username: &str, message: &str) -> bool {
        self.notify_friend_invite_decision(username, message)
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

    fn execute_invite_ui(
        &self,
        username: &str,
        password: Option<&str>,
        notification: &str,
    ) -> Result<BusinessInviteUiOutcome> {
        let residency = match self.active_ui_residency()? {
            super::UiResidency::Primary => UiResidencyTarget::Primary,
            super::UiResidency::SecondaryCurrentHall => UiResidencyTarget::SecondaryCurrentHall,
        };
        let outcome = self
            .invite_ui
            .submit(ExecuteInvite::new(
                username,
                password.map(str::to_string),
                fit_chat_message(notification),
                residency,
            ))
            .map_err(|error| anyhow!("邀请未进入 UI runtime: {error}"))?
            .wait()
            .map_err(|error| anyhow!("等待邀请 UI 结果失败: {error}"))?;
        let notification_warning = match outcome.notification() {
            InviteNotificationOutcome::Failed(failure) => {
                Some(format!("{}: {}", failure.stage(), failure.reason()))
            }
            InviteNotificationOutcome::NotAttempted | InviteNotificationOutcome::Sent => None,
        };
        let residency_failure = match outcome.residency() {
            crate::ui::routines::UiResidencyOutcome::Confirmed(_) => None,
            crate::ui::routines::UiResidencyOutcome::Failed(failure) => Some(failure.to_string()),
        };
        match outcome.effect() {
            InviteEffect::Entered => {
                self.on_entered_new_hall()?;
                if residency_failure.is_none() {
                    if let Err(error) = self.reply("BOT已经就绪,可以使用@麦克风指令了")
                    {
                        log::error!("邀请就绪消息发送失败: {error:#}");
                    }
                } else {
                    log::error!("邀请已经进入目标大厅，但未能确认最终监听驻留界面");
                }
                Ok(BusinessInviteUiOutcome::new(
                    true,
                    residency_failure,
                    notification_warning,
                ))
            }
            InviteEffect::ResultUnknown => Err(anyhow!(
                "邀请进入结果未知，禁止自动重试: {}",
                outcome
                    .failure()
                    .map_or("unknown stage", |failure| failure.stage())
            )),
            InviteEffect::NotAttempted => {
                let Some(failure) = outcome.failure() else {
                    return Ok(BusinessInviteUiOutcome::new(
                        false,
                        residency_failure,
                        notification_warning,
                    ));
                };
                if failure.certainty() == crate::runtime::ui::InputCertainty::ConfirmedFailure {
                    Ok(BusinessInviteUiOutcome::new(
                        false,
                        residency_failure,
                        notification_warning,
                    ))
                } else {
                    Err(anyhow!(failure.to_string()))
                }
            }
        }
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
