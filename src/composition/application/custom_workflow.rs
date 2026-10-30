use std::sync::atomic::Ordering as AtomicOrdering;
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
use anyhow::{Context, Result, anyhow};

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
        let service = self.business.custom_workflow.clone();
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
        while self.lifecycle.running.load(AtomicOrdering::SeqCst) && Instant::now() < deadline {
            let messages = self
                .wait_for_chat_decision_messages(
                    &mut reader,
                    poll_ms,
                    deadline.saturating_duration_since(Instant::now()),
                )
                .with_context(|| format!("{label}读取失败"))?;
            if !self.lifecycle.running.load(AtomicOrdering::SeqCst) {
                return Ok(ChatDecisionWait::Stopped);
            }
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
        if self.lifecycle.running.load(AtomicOrdering::SeqCst) {
            Ok(ChatDecisionWait::Timeout)
        } else {
            Ok(ChatDecisionWait::Stopped)
        }
    }

    fn notify_friend_invite_decision(&self, recipients: &[String], message: &str) -> bool {
        if recipients.is_empty() {
            log::error!("好友邀请确认回复失败: 没有可用的收件人名字");
            return false;
        }
        // 收件人主键沿用第一个候选，其余候选随投递一起下发用于搜列表。
        let outcome =
            self.send_friend_delivery_batch(&[FriendMessage::new(recipients[0].as_str(), message)
                .with_search_names(recipients.iter().cloned())]);
        match outcome {
            Ok(FriendBatchOutcome::Complete) => true,
            Ok(FriendBatchOutcome::Failed { failure, .. })
                if failure.kind() == FriendBatchFailureKind::ConfirmedUnsent =>
            {
                // 不打印 recipients：它是好友昵称/备注候选，属隐私面（MUST_READ §9）；
                // 数量足以说明「搜索时试了几个候选名」，具体名字留在配置里可查。
                log::error!(
                    "好友邀请确认回复失败（步骤：打开好友私聊），本次没有回复该好友，对方不会收到确认；搜索时用了 {} 个候选名：{}",
                    recipients.len(),
                    failure.reason()
                );
                false
            }
            Ok(FriendBatchOutcome::Failed { failure, .. }) => {
                log::error!("好友邀请确认回复失败: {}", failure.reason());
                false
            }
            Err(error) => {
                log::error!("好友邀请确认回复失败: {error:#}");
                false
            }
        }
    }

    fn wait_for_invite_decision(&self) -> Result<Option<bool>> {
        let live_config = self.lifecycle.live_configs.snapshot();
        match self.wait_for_chat_decision(
            "邀请确认",
            live_config.timing.invite.confirm_timeout_ms,
            live_config.timing.invite.confirm_poll_ms,
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
        self.business.business.set_commands_enabled(true)?;
        self.clear_hall_countdown_cache_for_new_visual_session("已进入新大厅")?;
        self.end_personal_playlist_for_context_loss("邀请流程已进入新大厅")?;
        Ok(())
    }

    pub(super) fn send_friend_message(&self, username: &str, message: &str) -> Result<bool> {
        log::info!("好友发言: {} -> {}", username, redacted_chat_text(&message));
        self.send_friend_delivery_routine(username, &message)
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
        let outcome = self.send_friend_delivery_batch(&[FriendMessage::new(username, message)])?;
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
        // 与 grouped 一一对应的候选名（搜好友列表用）。
        let mut grouped_search_names = Vec::<Vec<String>>::new();
        let mut raw_grouped = Vec::<(String, Vec<String>)>::new();
        for message in messages {
            let display_message = self
                .lifecycle
                .live_configs
                .identity
                .replace_display_names(message.message());
            let display_message = fit_chat_message(&display_message);
            let search_names = message.search_names().to_vec();
            if let Some((recipient, items)) = grouped.last_mut()
                && recipient == message.recipient()
            {
                items.push(display_message);
            } else {
                grouped.push((message.recipient().to_string(), vec![display_message]));
                grouped_search_names.push(search_names);
            }
            if let Some((recipient, items)) = raw_grouped.last_mut()
                && recipient == message.recipient()
            {
                items.push(message.message().to_string());
            } else {
                raw_grouped.push((
                    message.recipient().to_string(),
                    vec![message.message().to_string()],
                ));
            }
        }
        let request = SendFriendDeliveries::new(
            grouped
                .into_iter()
                .zip(grouped_search_names)
                .map(|((recipient, messages), names)| {
                    FriendDelivery::new(recipient, messages).with_search_names(names)
                })
                .collect(),
            residency,
        );
        let raw_request = SendFriendDeliveries::new(
            raw_grouped
                .into_iter()
                .map(|(recipient, messages)| FriendDelivery::new(recipient, messages))
                .collect(),
            residency,
        );
        let operation = match self.ui.friend_delivery_ui.submit(request.clone()) {
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
                    "好友消息已全部发送，但恢复值守驻留界面失败（步骤：发送完成后把界面切回聊天值守界面），本轮投递记为结果未知，需要下一轮确认界面状态后才能继续"
                );
                return Ok(FriendBatchOutcome::Failed {
                    retryable: Vec::new(),
                    failure: FriendBatchFailure::new(
                        FriendBatchFailureKind::ResultUnknown,
                        format!(
                            "好友消息已发送，但值守驻留恢复失败: {:?}",
                            outcome.residency()
                        ),
                    ),
                });
            }
            return Ok(FriendBatchOutcome::Complete);
        }

        let retryable = outcome
            .safe_retry_request(&raw_request)
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
            .ui
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
                let message = self
                    .lifecycle
                    .live_configs
                    .identity
                    .replace_display_names(&message);
                self.ui.chat_output.send_current_chat(&message)
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
                    .business
                    .begin_invite(InviteRequest::new(target.clone(), target, None, None))?
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

    fn notify_friend(&self, recipients: &[String], message: &str) -> bool {
        self.notify_friend_invite_decision(recipients, message)
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
        recipients: &[String],
        password: Option<&str>,
        notification: &str,
    ) -> Result<BusinessInviteUiOutcome> {
        let residency = match self.active_ui_residency()? {
            super::UiResidency::Primary => UiResidencyTarget::Primary,
            super::UiResidency::SecondaryCurrentHall => UiResidencyTarget::SecondaryCurrentHall,
        };
        let outcome = self
            .ui
            .invite_ui
            .submit(ExecuteInvite::new(
                recipients.to_vec(),
                password.map(str::to_string),
                fit_chat_message(
                    &self
                        .lifecycle
                        .live_configs
                        .identity
                        .replace_display_names(notification),
                ),
                residency,
            ))
            .map_err(|error| anyhow!("邀请未进入 UI runtime: {error}"))?
            .wait()
            .map_err(|error| anyhow!("等待邀请 UI 结果失败: {error}"))?;
        let mut notification_warning = match outcome.notification() {
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
                    if let Err(error) = self.reply_invite_ready("BOT已经就绪,可以使用@麦克风指令了")
                    {
                        log::error!("邀请就绪消息发送失败: {error:#}");
                        let warning = format!("邀请已进入，但就绪消息未确认发送: {error:#}");
                        notification_warning = Some(match notification_warning.take() {
                            Some(previous) => format!("{previous}; {warning}"),
                            None => warning,
                        });
                    }
                } else {
                    log::error!("邀请已经进入目标大厅，但未能确认最终值守驻留界面");
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
    if ["@邀请确认", "@确认邀请", "@同意邀请"]
        .iter()
        .any(|prefix| {
            command_text
                .strip_prefix(prefix)
                .is_some_and(|rest| decision_boundary(rest.chars().next()))
        })
    {
        Some(true)
    } else if ["@邀请拒绝", "@拒绝邀请"].iter().any(|prefix| {
        command_text
            .strip_prefix(prefix)
            .is_some_and(|rest| decision_boundary(rest.chars().next()))
    }) {
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
