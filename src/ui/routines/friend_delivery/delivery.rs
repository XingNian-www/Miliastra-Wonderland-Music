//! 文本分段、混合文本 / 表情发送执行与好友投递自动重试。
//!
//! 负责「已经定位到会话之后」的一切：分段合并、粘贴 / 键入、提交、发送后确认，
//! 以及邀请就绪闸门下的稳定确认；界面定位在 `navigation`，表情面板交互在
//! `emoji_panel`，导航与驻留收尾由模块根的事务包装器负责。

#[cfg(test)]
use miliastra_kernel::clock::Delay;
use miliastra_kernel::clock::{Clock, SystemClock};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use enigo::Key;

use crate::runtime::ocr::OcrRuntimeHandle;
use crate::runtime::recovery::GameRecoveryCoordinator;
use crate::runtime::ui::{
    InputCertainty, UiRoutineContext, UiRoutineFailure, UiRoutineProgressStage, UiStateKind,
    UiStateObservation,
};
use crate::text::emoji::{EmojiSegment, EmojiTag, ParsedReply, parse_reply};

use super::super::residency::{ConversationRef, ResidencyTarget, settle_to};
use super::super::state_observation::capture_normalized_ui_state;
use super::emoji_panel::send_emoji_segment;
use super::navigation::{ensure_secondary_chat, open_friend_conversation_with_recovery};
use super::{
    FriendDelivery, FriendDeliveryMessageStatus, FriendDeliveryOutcome,
    FriendDeliveryRoutineConfig, HallBatchInner, HallBatchStatus, InviteReadyGate,
    SendFriendDeliveries, SendHallBatch, UiResidencyTarget, after_input_failure,
    before_input_failure, settle_environment_without_ocr, wait_ms,
};

pub(crate) struct DeliveryAttemptFailure {
    failure: UiRoutineFailure,
    unknown_message_index: Option<usize>,
}
pub(crate) fn execute_friend_deliveries(
    context: &mut UiRoutineContext<'_>,
    request: SendFriendDeliveries,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    recovery: &GameRecoveryCoordinator,
) -> Vec<FriendDeliveryOutcome> {
    let recipient_count = request.deliveries.len();
    let mut outcomes = request
        .deliveries
        .iter()
        .map(|delivery| FriendDeliveryOutcome {
            message_statuses: vec![
                FriendDeliveryMessageStatus::NotAttempted;
                delivery.messages.len()
            ],
            failure: None,
        })
        .collect::<Vec<_>>();

    let start = ensure_secondary_chat(context, config, recovery);
    if let Err(failure) = start {
        if let Some(first) = outcomes.first_mut() {
            first.failure = Some(failure);
        }
    } else {
        for (recipient_index, delivery) in request.deliveries.iter().enumerate() {
            let mut retry = 0_u32;
            loop {
                context.publish_progress(UiRoutineProgressStage::LocatingFriend {
                    recipient_index: recipient_index + 1,
                    recipient_count,
                });
                match deliver_remaining_messages(
                    context,
                    ocr,
                    config,
                    recovery,
                    delivery,
                    &mut outcomes[recipient_index].message_statuses,
                    recipient_index,
                    recipient_count,
                ) {
                    Ok(()) => break,
                    Err(attempt)
                        if retry
                            < *config
                                .auto_retry_count
                                .read()
                                .expect("好友投递自动重试次数共享锁已中毒")
                            && retryable_certainty(attempt.failure.certainty()) =>
                    {
                        retry = retry.saturating_add(1);
                        // 首次重试只是 debug：这是流程内的正常兜底，玩家还没感知到失败。
                        // 反复重试才说明环境有问题，那时才值得进主日志（见下面升级逻辑）。
                        let limit = *config
                            .auto_retry_count
                            .read()
                            .expect("好友投递自动重试次数共享锁已中毒");
                        // 文案注意：这里**本次重试正要执行**，所以 retry==limit 时
                        // 说的是「最后一次自动重试」，不能说「已用尽」。
                        if retry < limit {
                            log::debug!(
                                "好友投递「{}」未送达，执行第 {}/{} 次自动重试（步骤：「{}」），本次失败可重试，流程继续：{}",
                                delivery.recipient(),
                                retry,
                                limit,
                                attempt.failure.stage_label(),
                                attempt.failure.reason()
                            );
                        } else {
                            log::warn!(
                                "好友投递「{}」已连续 {} 次未送达，第 {}/{} 次为最后一次自动重试（步骤：「{}」），若再失败本次投递将放弃：{}",
                                delivery.recipient(),
                                retry.saturating_sub(1),
                                retry,
                                limit,
                                attempt.failure.stage_label(),
                                attempt.failure.reason()
                            );
                        }
                    }
                    Err(attempt) => {
                        if let Some(index) = attempt.unknown_message_index {
                            outcomes[recipient_index].message_statuses[index] =
                                FriendDeliveryMessageStatus::ResultUnknown;
                        }
                        // 这里是**最终放弃**分支。以前完全没有日志：
                        // auto_retry_count 默认为 0 时根本进不了上面的重试分支，
                        // 于是投递失败在日志里彻底查不到（复核 P1-2 指出的漏报）。
                        log::warn!(
                            "好友投递放弃（步骤：「{}」）：收件人「{}」的这批消息本次未送达，不会自动重试，收件人需重新发起请求（{}）：{}",
                            attempt.failure.stage_label(),
                            delivery.recipient(),
                            attempt.failure.certainty().label(),
                            attempt.failure.reason()
                        );
                        outcomes[recipient_index].failure = Some(attempt.failure);
                        break;
                    }
                }
            }
            if outcomes[recipient_index].failure.is_some() {
                break;
            }
        }
    }

    context.publish_progress(UiRoutineProgressStage::RecoveringResidency);
    // 收尾（回到入口/逻辑驻留）由外层 `UiTask` 统一完成，内层不再第二次导航。
    outcomes
}
pub(crate) fn execute_hall_batch(
    context: &mut UiRoutineContext<'_>,
    request: SendHallBatch,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    recovery: &GameRecoveryCoordinator,
) -> HallBatchInner {
    let mut sent = 0_usize;
    let mut status = HallBatchStatus::Complete;
    let mut primary_chat_opened = false;

    if !*config
        .send_enabled
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        || request.messages.is_empty()
    {
        sent = request.messages.len();
    } else if let Err(failure) = prepare_hall_batch_send_location(
        context,
        config,
        recovery,
        request.residency,
        request.ready.as_ref(),
        &mut primary_chat_opened,
    ) {
        status = HallBatchStatus::Failed(failure);
    } else {
        for (index, message) in request.messages.iter().enumerate() {
            if index > 0 {
                if let Err(failure) = wait_ms(
                    context,
                    request.delay_ms,
                    "wait_hall_batch",
                    InputCertainty::AfterInputUnknown,
                ) {
                    status = HallBatchStatus::Failed(failure);
                    break;
                }
            }
            let send_result = send_chat_message_with_emoji(
                context,
                ocr,
                config,
                message,
                request.residency,
                request.ready.as_ref(),
            );
            match send_result {
                Ok(()) => sent += 1,
                Err(failure) => {
                    status = HallBatchStatus::Failed(failure);
                    break;
                }
            }
        }
    }

    if request.ready.is_some() && matches!(&status, HallBatchStatus::Failed(_)) {
        // 邀请就绪（ready 闸门）取消/失败：发送阶段已经证明“不能再动界面”，
        // 收尾阶段也必须零输入——否则外层 `UiTask` 会按 `RestoreEntry`
        // 发 Esc/Return/点击会话行，违背“取消时零输入”（B15-P1-5）。
        context.force_finish();
    }
    HallBatchInner {
        sent,
        status,
        primary_chat_opened,
    }
}
/// 把界面准备到"可发送的二级聊天（当前大厅聊天）"。
///
/// 这是发送位置的会话级前置，不是任务收尾：收尾统一由包装器完成。
/// `recovery` 与包装器同源：前置里的两次 `settle_to` 也记在同一份协调器上。
pub(crate) fn prepare_hall_batch_send_location(
    context: &mut UiRoutineContext<'_>,
    config: &FriendDeliveryRoutineConfig,
    recovery: &GameRecoveryCoordinator,
    target: UiResidencyTarget,
    ready: Option<&InviteReadyGate>,
    primary_chat_opened: &mut bool,
) -> std::result::Result<(), UiRoutineFailure> {
    if let Some(gate) = ready {
        check_invite_ready_running(gate)?;
    }
    context
        .device()
        .ensure_ready(config.after_activate_ms)
        .map_err(|error| before_input_failure("prepare_hall_batch", error))?;
    if let Some(gate) = ready {
        // 邀请就绪：闸门与一级稳定必须先于导航（包装器已用 AssumeTarget 跳过导航）。
        check_invite_ready_running(gate)?;
        settle_to(
            context,
            &settle_environment_without_ocr(config, recovery),
            ResidencyTarget::Primary,
        )?;
        confirm_invite_primary_stability(context, config, gate)?;
        if target == UiResidencyTarget::Primary {
            check_invite_ready_running(gate)?;
            context
                .device()
                .press_key(Key::Return)
                .map_err(|error| after_input_failure("open_primary_chat", error))?;
            *primary_chat_opened = true;
            wait_ms(
                context,
                config.open_chat_ms,
                "open_primary_chat",
                InputCertainty::AfterInputUnknown,
            )?;
        }
    }
    // 发送位置固定为"二级聊天的当前大厅"：索引/点击大厅行是会话级动作，
    // 包装器的 kind 级导航不替代它；这里只做发送前置，不承担任务收尾。
    settle_to(
        context,
        &settle_environment_without_ocr(config, recovery),
        ResidencyTarget::Secondary(ConversationRef::CurrentHall),
    )?;
    Ok(())
}
pub(crate) fn retryable_certainty(certainty: InputCertainty) -> bool {
    matches!(
        certainty,
        InputCertainty::BeforeInput | InputCertainty::ConfirmedFailure
    )
}
pub(crate) fn deliver_remaining_messages(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    recovery: &GameRecoveryCoordinator,
    delivery: &FriendDelivery,
    statuses: &mut [FriendDeliveryMessageStatus],
    recipient_index: usize,
    recipient_count: usize,
) -> std::result::Result<(), DeliveryAttemptFailure> {
    // 收件人可能只是 UID（外部源），真正能搜列表的名字在 search_names 里。
    let recipients = delivery.search_recipients();
    open_friend_conversation_with_recovery(context, ocr, config, recovery, &recipients)
        .map_err(safe_attempt_failure)?;

    for (message_index, (message, status)) in delivery
        .messages
        .iter()
        .zip(statuses.iter_mut())
        .enumerate()
    {
        if *status != FriendDeliveryMessageStatus::NotAttempted {
            continue;
        }
        context.publish_progress(UiRoutineProgressStage::SendingFriendMessage {
            recipient_index: recipient_index + 1,
            recipient_count,
            message_index: message_index + 1,
            message_count: delivery.messages.len(),
        });
        send_current_chat_message(context, config, message).map_err(|failure| {
            let unknown_message_index =
                (failure.certainty() == InputCertainty::AfterInputUnknown).then_some(message_index);
            DeliveryAttemptFailure {
                failure,
                unknown_message_index,
            }
        })?;
        *status = FriendDeliveryMessageStatus::Sent;
    }
    Ok(())
}

fn safe_attempt_failure(failure: UiRoutineFailure) -> DeliveryAttemptFailure {
    DeliveryAttemptFailure {
        failure,
        unknown_message_index: None,
    }
}
pub(crate) fn send_current_chat_message(
    context: &mut UiRoutineContext<'_>,
    config: &FriendDeliveryRoutineConfig,
    message: &str,
) -> std::result::Result<(), UiRoutineFailure> {
    send_current_chat_message_guarded(context, config, message, None)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChatInputStep {
    VerifySecondary,
    Focus,
    Input,
    Submit,
}

pub(crate) fn run_verified_chat_input(
    mut action: impl FnMut(ChatInputStep) -> Result<(), UiRoutineFailure>,
) -> Result<(), UiRoutineFailure> {
    for step in [
        ChatInputStep::VerifySecondary,
        ChatInputStep::Focus,
        ChatInputStep::VerifySecondary,
        ChatInputStep::Input,
        ChatInputStep::VerifySecondary,
        ChatInputStep::Submit,
    ] {
        action(step)?;
    }
    Ok(())
}
pub(crate) fn send_current_chat_message_guarded(
    context: &mut UiRoutineContext<'_>,
    config: &FriendDeliveryRoutineConfig,
    message: &str,
    ready: Option<&InviteReadyGate>,
) -> Result<(), UiRoutineFailure> {
    if !*config
        .send_enabled
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        return Ok(());
    }
    run_verified_chat_input(|step| {
        if let Some(gate) = ready {
            check_invite_ready_running(gate)?;
        }
        match step {
            ChatInputStep::VerifySecondary => {
                if ready.is_some() {
                    // 每次capture都会刷新模板分类，不能使用打开聊天前的旧状态。
                    capture_normalized_ui_state(
                        context,
                        &config.state_observation(),
                        "confirm_ready_secondary",
                        InputCertainty::ConfirmedFailure,
                    )?;
                    match context.latest_ui_state() {
                        Some(UiStateObservation::Classified(state))
                            if state.classification().kind() == UiStateKind::Secondary
                                && state.stable_kind() == Some(UiStateKind::Secondary) => {}
                        _ => {
                            return Err(UiRoutineFailure::new(
                                InputCertainty::ConfirmedFailure,
                                "confirm_ready_secondary",
                                "secondary chat is not freshly confirmed; readiness text will not be sent",
                            ));
                        }
                    }
                    check_invite_ready_running(ready.expect("checked above"))?;
                }
            }
            ChatInputStep::Focus => {
                context
                    .device()
                    .click_point(config.chat_click.x, config.chat_click.y)
                    .map_err(|error| after_input_failure("focus_friend_message", error))?;
                wait_ms(
                    context,
                    config.click_ms,
                    "focus_friend_message",
                    InputCertainty::AfterInputUnknown,
                )?;
            }
            ChatInputStep::Input => {
                if let Err(paste_error) = context.device().paste_text(message, config.text_ms) {
                    if ready.is_some() {
                        // 输入可能已经部分生效；就绪消息不能盲目键入重放，也不能继续按发送键。
                        return Err(UiRoutineFailure::new(
                            InputCertainty::AfterInputUnknown,
                            "input_invite_ready_message",
                            format!("readiness paste failed; do not replay: {paste_error:#}"),
                        ));
                    }
                    if let Err(input_error) = context.device().input_text(message, config.text_ms) {
                        return Err(UiRoutineFailure::new(
                            InputCertainty::AfterInputUnknown,
                            "input_friend_message",
                            format!(
                                "paste failed: {paste_error:#}; text input failed: {input_error:#}"
                            ),
                        ));
                    }
                }
            }
            ChatInputStep::Submit => {
                context.device().press_key(Key::Return).map_err(|error| {
                    UiRoutineFailure::new(
                        InputCertainty::AfterInputUnknown,
                        "send_friend_message",
                        format!("{error:#}"),
                    )
                })?;
                wait_ms(
                    context,
                    config.send_ms,
                    "send_friend_message",
                    InputCertainty::AfterInputUnknown,
                )?;
            }
        }
        Ok(())
    })
}
pub(crate) fn check_invite_ready_running(gate: &InviteReadyGate) -> Result<(), UiRoutineFailure> {
    if gate.running.load(Ordering::SeqCst) {
        Ok(())
    } else {
        Err(UiRoutineFailure::new(
            InputCertainty::ConfirmedFailure,
            "invite_ready_cancelled",
            "invitation readiness sending was cancelled",
        ))
    }
}
#[cfg(test)]
pub(crate) fn wait_for_invite_primary_stability(
    clock: &dyn Clock,
    delay: &dyn Delay,
    timeout: Duration,
    poll: Duration,
    running: &AtomicBool,
    mut observe: impl FnMut() -> Result<bool, UiRoutineFailure>,
) -> Result<(), UiRoutineFailure> {
    wait_for_invite_primary_stability_with_port(clock, timeout, poll, running, |wait| {
        if let Some(duration) = wait {
            delay.wait(duration);
            Ok(false)
        } else {
            observe()
        }
    })
}

pub(crate) fn wait_for_invite_primary_stability_with_port(
    clock: &dyn Clock,
    timeout: Duration,
    poll: Duration,
    running: &AtomicBool,
    mut port: impl FnMut(Option<Duration>) -> Result<bool, UiRoutineFailure>,
) -> Result<(), UiRoutineFailure> {
    let deadline = clock.now() + timeout;
    let mut stable_since = None;
    let mut last_sample = None;
    let poll = poll.clamp(Duration::from_millis(10), Duration::from_millis(100));
    loop {
        if !running.load(Ordering::SeqCst) {
            return Err(UiRoutineFailure::new(
                InputCertainty::ConfirmedFailure,
                "invite_ready_cancelled",
                "cancelled before readiness confirmation",
            ));
        }
        let primary = port(None).unwrap_or(false);
        let now = clock.now();
        if !running.load(Ordering::SeqCst) {
            return Err(UiRoutineFailure::new(
                InputCertainty::ConfirmedFailure,
                "invite_ready_cancelled",
                "cancelled during readiness observation",
            ));
        }
        // 长时间没有新观测也不能累计稳定时间。
        if last_sample
            .is_some_and(|last| now.saturating_duration_since(last) > Duration::from_secs(1))
        {
            stable_since = None;
        }
        last_sample = Some(now);
        if primary {
            let since = *stable_since.get_or_insert(now);
            if now <= deadline && now.saturating_duration_since(since) >= Duration::from_secs(2) {
                return Ok(());
            }
        } else {
            stable_since = None;
        }
        if now >= deadline {
            return Err(UiRoutineFailure::new(
                InputCertainty::ConfirmedFailure,
                "confirm_invite_primary_stability",
                "primary UI was not continuously confirmed for two seconds before timeout",
            ));
        }
        port(Some(poll.min(deadline.saturating_duration_since(now))))?;
    }
}
pub(crate) fn confirm_invite_primary_stability(
    context: &mut UiRoutineContext<'_>,
    config: &FriendDeliveryRoutineConfig,
    gate: &InviteReadyGate,
) -> Result<(), UiRoutineFailure> {
    context.publish_progress(UiRoutineProgressStage::ConfirmingUi);
    wait_for_invite_primary_stability_with_port(
        &SystemClock,
        Duration::from_millis(config.timeout_ms).max(Duration::from_secs(3)),
        Duration::from_millis(config.poll_ms),
        &gate.running,
        |wait| {
            if let Some(duration) = wait {
                context.wait_cancellable(duration).map_err(|failure| {
                    UiRoutineFailure::new(
                        InputCertainty::ConfirmedFailure,
                        "confirm_invite_primary_stability",
                        failure.reason(),
                    )
                })?;
                return Ok(false);
            }
            capture_normalized_ui_state(
                context,
                &config.state_observation(),
                "confirm_invite_primary_stability",
                InputCertainty::ConfirmedFailure,
            )?;
            Ok(
                matches!(context.latest_ui_state(),Some(UiStateObservation::Classified(state)) if state.classification().kind()==UiStateKind::Primary && state.stable_kind()==Some(UiStateKind::Primary)),
            )
        },
    )
}
/// 一次性发送入口：允许表情时按标签拆段，否则保持原有纯文本行为。
pub(crate) fn send_chat_message_with_emoji(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    message: &str,
    residency: UiResidencyTarget,
    ready: Option<&InviteReadyGate>,
) -> Result<(), UiRoutineFailure> {
    let parsed = match parse_reply(message) {
        Ok(parsed) => parsed,
        Err(error) => {
            // 语法错误不在发送层猜测：按原文本发送，配置面板保存时应已拦住。
            log::warn!("表情标签解析失败，按纯文本发送：{error}");
            return send_current_chat_message_guarded(context, config, message, ready);
        }
    };
    if !parsed.has_emoji() {
        return send_current_chat_message_guarded(context, config, message, ready);
    }
    // 表情只在二级当前聊天执行；关闭开关或在其它位置发送时，标签一律剥离，
    // 避免把 `{表情包:...}` 这样的控制文本当成普通聊天发出去。
    let emoji_allowed =
        config.emoji_enabled && residency == UiResidencyTarget::SecondaryCurrentHall;
    send_parsed_segments(context, ocr, config, &parsed, ready, emoji_allowed)
}

pub(crate) fn send_parsed_segments(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    parsed: &ParsedReply,
    ready: Option<&InviteReadyGate>,
    emoji_allowed: bool,
) -> Result<(), UiRoutineFailure> {
    // 文字先合并成**一条**消息，再按顺序补发表情。
    //
    // 表情点击即发送，无法与文字同条；如果按标签原位置拆文字，聊天里会出现
    // 「哼，声音也放好了，」「示例玩家A听完快去睡。」这种无意义断行。因此把文字
    // 全部拼成一条，表情统一挪到这条文字之后发送。
    let mut text = String::new();
    let mut tags: Vec<&EmojiTag> = Vec::new();
    for segment in &parsed.segments {
        match segment {
            EmojiSegment::Text(value) => text.push_str(value),
            EmojiSegment::Emoji(tag) => tags.push(tag),
        }
    }
    if !text.trim().is_empty() {
        send_current_chat_message_guarded(context, config, &text, ready)?;
    }
    if !emoji_allowed {
        if !tags.is_empty() {
            log::debug!(
                "表情标签已剥离（表情开关关闭或不在二级当前聊天）：{} 个",
                tags.len()
            );
        }
        return Ok(());
    }
    dispatch_emoji_tags(&tags, ready, !text.trim().is_empty(), |tag| {
        send_emoji_segment(context, ocr, config, tag, "send_emoji")
    })
}

/// 依次发送表情标签；每一步之前都重新确认邀请就绪闸门仍开启。
///
/// 表情点击即发送、无法撤回：文本段发完到表情段之间可能已经过去几十毫秒，
/// 这期间邀请任务被取消是真实可达的；不复查闸门就会继续打开表情面板并点击
/// （B15-P1-5）。闸门关闭时**立即**返回失败（不再尝试剩余标签），
/// 保持“取消时零输入”的硬约束。
///
/// 部分标签发送失败沿用原语义：只要文字段已经发出（`text_present`），
/// 就不再把这个局部失败升级成整条消息失败。
pub(super) fn dispatch_emoji_tags(
    tags: &[&EmojiTag],
    ready: Option<&InviteReadyGate>,
    text_present: bool,
    mut send: impl FnMut(&EmojiTag) -> Result<(), UiRoutineFailure>,
) -> Result<(), UiRoutineFailure> {
    let mut first_emoji_failure: Option<UiRoutineFailure> = None;
    for tag in tags {
        if let Some(gate) = ready {
            check_invite_ready_running(gate)?;
        }
        if let Err(failure) = send(tag) {
            log::warn!(
                "表情发送失败（页签 {}，候选 {} 个，原因：{}）",
                tag.pack,
                tag.candidates.len(),
                failure.reason()
            );
            if first_emoji_failure.is_none() {
                first_emoji_failure = Some(failure);
            }
        }
    }
    match first_emoji_failure {
        Some(failure) if !text_present => Err(failure),
        _ => Ok(()),
    }
}
