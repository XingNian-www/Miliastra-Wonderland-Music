use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use super::friend_delivery::{
    FriendDeliveryRoutineConfig, UiResidencyOutcome, after_input_failure, before_input_failure,
    capture_normalized, wait_ms,
};
use super::residency::{ConversationRef, ResidencyTarget, SettleEnvironment};
use super::transaction::{EntryPolicy, FinishPolicy, UiTask, UiTaskSpec};
use crate::observation::chat::{
    ChatIdentity, ChatTimeline, ChatTimelineSnapshot, FriendUnreadLayout, ObservedChatMessageId,
    UnreadFriendHit, find_unread_friend_hits, latest_incoming_fingerprint,
    unread_hit_still_visible,
};
use crate::runtime::ocr::OcrRuntimeHandle;
use crate::runtime::recovery::GameRecoveryCoordinator;
use crate::runtime::ui::{
    InputCertainty, UiOperation, UiRoutine, UiRoutineContext, UiRoutineFailure, UiRuntimeHandle,
    UiSubmitError, sealed,
};

#[derive(Clone, Debug)]
pub(crate) struct ProcessSecondaryUnread {
    hit: UnreadFriendHit,
    discard_only: bool,
}

impl ProcessSecondaryUnread {
    pub(crate) const fn new(hit: UnreadFriendHit, discard_only: bool) -> Self {
        Self { hit, discard_only }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum SecondaryUnreadEffect {
    Message {
        captured_at: Instant,
        friend_name: String,
        text: String,
    },
    Discarded,
    Stale,
    NoMessage,
    Failed(UiRoutineFailure),
}

#[derive(Clone, Debug)]
pub(crate) struct ProcessSecondaryUnreadOutcome {
    effect: SecondaryUnreadEffect,
    residency: UiResidencyOutcome,
}

impl ProcessSecondaryUnreadOutcome {
    pub(crate) fn effect(&self) -> &SecondaryUnreadEffect {
        &self.effect
    }

    pub(crate) fn residency(&self) -> &UiResidencyOutcome {
        &self.residency
    }
}

#[derive(Clone)]
pub(crate) struct SecondaryUnreadUi {
    runtime: UiRuntimeHandle,
    ocr: OcrRuntimeHandle,
    chat_timeline: ChatTimeline,
    config: SecondaryUnreadRoutineConfig,
    /// 进程级共享的恢复协调器（见 [`SettleEnvironment::with_recovery`]）。
    recovery: GameRecoveryCoordinator,
}

impl SecondaryUnreadUi {
    pub(crate) fn new(
        runtime: UiRuntimeHandle,
        ocr: OcrRuntimeHandle,
        config: SecondaryUnreadRoutineConfig,
        chat_timeline: ChatTimeline,
    ) -> Self {
        Self {
            runtime,
            ocr,
            chat_timeline,
            config,
            recovery: GameRecoveryCoordinator::default(),
        }
    }

    /// 用进程级共享的恢复协调器替换默认实例（装配层只 `default()` 一次）。
    pub(crate) fn with_recovery(mut self, recovery: GameRecoveryCoordinator) -> Self {
        self.recovery = recovery;
        self
    }

    pub(crate) fn submit(
        &self,
        request: ProcessSecondaryUnread,
    ) -> Result<UiOperation<ProcessSecondaryUnreadOutcome>, UiSubmitError> {
        self.runtime.submit(ProcessSecondaryUnreadTask {
            request,
            ocr: self.ocr.clone(),
            chat_timeline: self.chat_timeline.clone(),
            config: self.config.clone(),
            recovery: self.recovery.clone(),
        })
    }
}

#[derive(Clone)]
pub(crate) struct SecondaryUnreadRoutineConfig {
    residency: FriendDeliveryRoutineConfig,
    unread_layout: FriendUnreadLayout,
    bubble_poll_ms: Arc<RwLock<u64>>,
}

impl SecondaryUnreadRoutineConfig {
    pub(crate) fn resolve(
        residency: FriendDeliveryRoutineConfig,
        _same_line_y_tolerance: i32,
        change_debounce_ms: Arc<RwLock<u64>>,
    ) -> Self {
        let unread_layout = FriendUnreadLayout::resolve(
            residency.screen.expected_width,
            residency.screen.expected_height,
            residency.friend_list_region(),
        );
        Self {
            residency,
            unread_layout,
            bubble_poll_ms: change_debounce_ms,
        }
    }
}

struct ProcessSecondaryUnreadRoutine {
    request: ProcessSecondaryUnread,
    ocr: OcrRuntimeHandle,
    chat_timeline: ChatTimeline,
    config: SecondaryUnreadRoutineConfig,
    recovery: GameRecoveryCoordinator,
}

impl ProcessSecondaryUnreadRoutine {
    /// 业务前置恢复同样挂在进程级协调器上：这次基线也属于「恢复采样」。
    fn environment(&self) -> SettleEnvironment {
        SettleEnvironment::new(self.ocr.clone(), self.config.residency.clone())
            .with_recovery(self.recovery.clone())
    }
}

impl sealed::UiRoutineSealed for ProcessSecondaryUnreadRoutine {}

impl UiRoutine for ProcessSecondaryUnreadRoutine {
    type Output = ProcessSecondaryUnreadOutcome;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        // 抑制守卫由外层 `ProcessSecondaryUnreadTask` 持有（见那里的说明）：
        // 本结构体在 `UiTask` 的内层执行结束时就会被消费掉，若守卫挂在这里，
        // 它会在包装器的收尾恢复**开始之前**析构（B15-P1-1）。
        let environment = self.environment();
        let effect = match process_unread(
            context,
            &self.request,
            &environment,
            &self.chat_timeline,
            &self.config,
        ) {
            Ok(effect) => effect,
            Err(failure) => SecondaryUnreadEffect::Failed(failure),
        };
        // 收尾固定为包装器的 `Fixed(Secondary(CurrentHall))`；内层不再自己
        // 调恢复，避免“例程结尾一次 + 包装器一次”的重复导航。
        ProcessSecondaryUnreadOutcome {
            effect,
            residency: UiResidencyOutcome::Failed(UiRoutineFailure::new(
                InputCertainty::BeforeInput,
                "ui_task_finish",
                "收尾由 UiTask 的 Fixed(SecondaryCurrentHall) 负责",
            )),
        }
    }
}

/// 二级好友未读任务的价值基线固定为“当前大厅二级”。
///
/// 收尾只由包装器的 `Fixed(Secondary(CurrentHall))` 执行一次；例程开头那一次
/// `settle_to` 只是“红线/未读位置必须在当前大厅上才有意义”的业务前置，
/// 不是收尾。
fn process_secondary_unread_task_spec(environment: SettleEnvironment) -> UiTaskSpec {
    UiTaskSpec::new(
        ResidencyTarget::Secondary(ConversationRef::CurrentHall),
        FinishPolicy::Fixed(ResidencyTarget::Secondary(ConversationRef::CurrentHall)),
        environment,
    )
    .with_entry(EntryPolicy::Passthrough)
}

fn task_not_started_failure(failure: Option<UiRoutineFailure>) -> UiRoutineFailure {
    failure.unwrap_or_else(|| {
        UiRoutineFailure::new(
            InputCertainty::Cancelled,
            "ui_task_start",
            "UI 任务在开始前已取消，内层例程没有执行",
        )
    })
}

struct ProcessSecondaryUnreadTask {
    request: ProcessSecondaryUnread,
    ocr: OcrRuntimeHandle,
    chat_timeline: ChatTimeline,
    config: SecondaryUnreadRoutineConfig,
    recovery: GameRecoveryCoordinator,
}

impl ProcessSecondaryUnreadTask {
    /// 收尾环境必须挂着进程级共享的协调器，否则本次恢复记账只活在本任务里。
    fn environment(&self) -> SettleEnvironment {
        SettleEnvironment::new(self.ocr.clone(), self.config.residency.clone())
            .with_recovery(self.recovery.clone())
    }
}

impl sealed::UiRoutineSealed for ProcessSecondaryUnreadTask {}

impl UiRoutine for ProcessSecondaryUnreadTask {
    type Output = ProcessSecondaryUnreadOutcome;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        // 抑制守卫必须在**任何导航之前**取得，并活到 `UiTask::execute` 返回——
        // 那次返回已经包含包装器的 `Fixed(Secondary(CurrentHall))` 收尾恢复
        // （含失败/取消路径）。放在这里而不是内层 `execute`，是因为内层返回时
        // 收尾还没开始，守卫会被提前析构（B15-P1-1）。
        let discard_guard = if self.request.discard_only {
            match self.chat_timeline.suppress_friend_business() {
                Ok(guard) => Some(guard),
                Err(error) => {
                    let failure = before_input_failure("suppress_secondary_unread", error);
                    return ProcessSecondaryUnreadOutcome {
                        effect: SecondaryUnreadEffect::Failed(failure.clone()),
                        residency: UiResidencyOutcome::Failed(failure),
                    };
                }
            }
        } else {
            None
        };
        let environment = self.environment();
        let outcome = UiTask::new(
            process_secondary_unread_task_spec(environment),
            ProcessSecondaryUnreadRoutine {
                request: self.request,
                ocr: self.ocr,
                chat_timeline: self.chat_timeline,
                config: self.config,
                recovery: self.recovery,
            },
        )
        .execute(context);
        // 收尾恢复已经随 `UiTask::execute` 结束，这里才允许释放抑制。
        drop(discard_guard);
        let residency = outcome.residency.clone();
        match outcome.output {
            Some(mut output) => {
                output.residency = residency;
                output
            }
            None => {
                let failure = task_not_started_failure(outcome.entry_failure);
                ProcessSecondaryUnreadOutcome {
                    effect: SecondaryUnreadEffect::Failed(failure.clone()),
                    residency: UiResidencyOutcome::Failed(failure),
                }
            }
        }
    }
}

fn process_unread(
    context: &mut UiRoutineContext<'_>,
    request: &ProcessSecondaryUnread,
    environment: &SettleEnvironment,
    chat_timeline: &ChatTimeline,
    config: &SecondaryUnreadRoutineConfig,
) -> Result<SecondaryUnreadEffect, UiRoutineFailure> {
    context
        .device()
        .ensure_ready(config.residency.after_activate_ms)
        .map_err(|error| before_input_failure("prepare_secondary_unread", error))?;
    // 业务前置：未读红点的行坐标只对“当前大厅”有意义；这里只建立一次基线，
    // 不承担任务收尾。收尾由包装器 Fixed(Secondary(CurrentHall)) 唯一执行。
    super::residency::settle_to(
        context,
        environment,
        ResidencyTarget::Secondary(ConversationRef::CurrentHall),
    )?;

    let Some(hit) = reacquire_unread_hit(context, config)? else {
        log::debug!(
            "二级好友未读任务执行前红点已失效：排队时记录的行位置为 {}，该未读已被其它路径消费或界面已滚动，本次任务不再执行",
            request.hit.row_click.y
        );
        return Ok(SecondaryUnreadEffect::Stale);
    };
    if hit.row_click.y != request.hit.row_click.y {
        log::debug!(
            "二级好友未读任务已重新定位红点：排队时为 {}，当前为 {}，按当前行继续处理",
            request.hit.row_click.y,
            hit.row_click.y
        );
    }

    let mut opened = false;
    for _ in 0..2 {
        context
            .device()
            .click_point(hit.row_click.x, hit.row_click.y)
            .map_err(|error| after_input_failure("open_secondary_unread", error))?;
        wait_ms(
            context,
            config.residency.click_ms,
            "open_secondary_unread",
            InputCertainty::AfterInputUnknown,
        )?;
        let image = capture_normalized(
            context,
            &config.residency,
            "confirm_secondary_unread_opened",
            InputCertainty::AfterInputUnknown,
        )?;
        if !unread_hit_still_visible(&image, &hit, &config.unread_layout) {
            opened = true;
            break;
        }
    }
    if !opened {
        return Ok(SecondaryUnreadEffect::NoMessage);
    }
    let opened_at = Instant::now();
    let deadline = opened_at + Duration::from_millis(config.residency.timeout_ms());
    loop {
        if !request.discard_only {
            wait_bubble_stable(context, config, deadline)?;
        }
        let Some(snapshot) =
            wait_shared_friend_snapshot(context, chat_timeline, opened_at, deadline)?
        else {
            return Err(unread_message_failure(
                "fresh shared friend chat OCR did not arrive before timeout",
            ));
        };
        let Some(ChatIdentity::Friend(friend_name)) = snapshot.chat.as_ref() else {
            unreachable!("wait_shared_friend_snapshot only returns confirmed friend chats");
        };
        if request.discard_only {
            // 初始红点清场（进入二级值守时那一次）：只建立基线、清红点，历史一律不执行。
            // 必须在"提升最后一条命令"之前返回；清场全程还持有好友业务抑制
            // （execute 里的 suppress_friend_business），两层都不允许它派发任何命令。
            //
            // 确认带本快照的 `captured_at`：清场是「这一屏看到的历史都不执行」，
            // 不是「这个 id 从此作废」。会话在后续帧里若把某条气泡修正成新命令，
            // 那次修正会推进修订围栏，无时间的永久 ACK 会把它一起吞掉。
            for message in &snapshot.messages {
                chat_timeline
                    .acknowledge_business_at(&message.id, snapshot.captured_at)
                    .map_err(|error| after_input_failure("discard_secondary_unread", error))?;
            }
            return Ok(SecondaryUnreadEffect::Discarded);
        }
        if let Some(message) = latest_unread_message(&snapshot) {
            // 用户规则：进入有未读的会话时读取最后发送的那条命令，恰好一次。
            // 提升只把这条气泡放行给共享观察的业务订阅者；这里必须确认它真的被取走，
            // 否则 UI 事务立刻恢复大厅，这次提升就没有可消费的后续帧，最后什么都不派发。
            let promoted = chat_timeline
                .deliver_unread_baseline(&snapshot, &message.id)
                .map_err(|error| after_input_failure("deliver_secondary_unread", error))?;
            if promoted {
                wait_promoted_unread_consumed(context, chat_timeline, &message.id, deadline)?;
            } else {
                // 不在首屏基线里：要么这条气泡已按常规路径交付过（成功命令锁或永久 ACK），
                // 要么它从未进过基线，此时不会有任何派发。如实记录以区分这两种情况，
                // 不在这里自行发明"再读一次"的规则。
                //
                // 判据必须带本快照的 `captured_at`：这份修订若已被更新的修订推进围栏，
                // 账本里那一条仍是待处理的（新修订还要交付），只查账本会说"仍在队列里"，
                // 让排查误以为是消费端卡住了。
                log::warn!(
                    "二级好友未读提升未生效（步骤：提升未读消息）：好友「{}」的这条气泡不在首屏基线内，无法据此派发命令；该气泡可能已按常规路径交付过（{}）",
                    friend_name,
                    if chat_timeline
                        .is_business_pending_at(&message.id, snapshot.captured_at)
                        .unwrap_or(false)
                    {
                        "这份修订仍在待处理队列中，等待业务链路消费"
                    } else {
                        "这份修订不再等待派发：已被消费，或已被更新的修订取代"
                    }
                );
            }
            return Ok(SecondaryUnreadEffect::Message {
                captured_at: snapshot.captured_at,
                friend_name: friend_name.to_string(),
                text: message.text.clone(),
            });
        }
        if Instant::now() >= deadline {
            return Err(unread_message_failure(
                "shared incoming bubble OCR remained empty until timeout",
            ));
        }
        wait_ms(
            context,
            bubble_poll_ms(config),
            "wait_secondary_unread_message",
            InputCertainty::AfterInputUnknown,
        )?;
    }
}

fn wait_shared_friend_snapshot(
    context: &mut UiRoutineContext<'_>,
    chat_timeline: &ChatTimeline,
    opened_at: Instant,
    deadline: Instant,
) -> Result<Option<ChatTimelineSnapshot>, UiRoutineFailure> {
    let mut token = chat_timeline
        .request_refresh()
        .map_err(|error| after_input_failure("refresh_secondary_unread", error))?;
    loop {
        if let Some(snapshot) = chat_timeline
            .wait_for_snapshot(token, Duration::ZERO)
            .map_err(|error| after_input_failure("ocr_secondary_unread", error))?
        {
            if fresh_friend_snapshot(&snapshot, opened_at) {
                return Ok(Some(snapshot));
            }
            token = chat_timeline
                .request_refresh()
                .map_err(|error| after_input_failure("refresh_secondary_unread", error))?;
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        context
            .wait_cancellable(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(25)),
            )
            .map_err(|failure| {
                UiRoutineFailure::new(
                    InputCertainty::AfterInputUnknown,
                    "wait_secondary_unread_message",
                    failure.reason(),
                )
            })?;
    }
}

fn latest_unread_message(
    snapshot: &ChatTimelineSnapshot,
) -> Option<&crate::observation::chat::ChatTimelineMessage> {
    snapshot
        .messages
        .iter()
        .filter(|message| message.message_type == "pink")
        .max_by_key(|message| message.block.y)
        .filter(|message| unread_text_is_usable(&message.text))
}

/// 等待业务订阅者取走这次未读提升。
///
/// 提升写入的是一份共享观察资格修订（`deliver_unread_baseline`），真正的消费发生在
/// 值守循环的业务订阅者里。UI 事务若在提升后立刻恢复大厅，这条唯一候选就没有后续帧，
/// 可能一直没人消费——用户规则是"进入有未读的会话读取最后发送的那条命令"，
/// 而不是"提升了却不用"。超时不当作失败：消费方可能已按常规路径处理过同一气泡。
///
/// 这里问的是**消费进度**，不是"这份快照该不该交付"：提升刚把围栏推到本次修订，
/// 任何能消费它的读者捕获时间都不会早于围栏，所以按账本作答与按围栏作答结果相同；
/// 而账本探针语义更直白——「这条 id 还被待处理队列占着吗」。
fn wait_promoted_unread_consumed(
    context: &mut UiRoutineContext<'_>,
    chat_timeline: &ChatTimeline,
    id: &ObservedChatMessageId,
    deadline: Instant,
) -> Result<(), UiRoutineFailure> {
    let mut token = chat_timeline
        .request_refresh()
        .map_err(|error| after_input_failure("refresh_secondary_unread", error))?;
    loop {
        if !chat_timeline
            .is_business_pending_in_ledger(id)
            .map_err(|error| after_input_failure("confirm_secondary_unread", error))?
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            log::warn!(
                "二级好友未读消息在超时前未被消费（步骤：确认二级未读消息）：该气泡是二级会话的第 {} 条，消费者未在期限内取走，本次未读提升放弃且不会重试",
                id.bubble_sequence.get()
            );
            return Ok(());
        }
        if chat_timeline
            .wait_for_snapshot(token, Duration::ZERO)
            .map_err(|error| after_input_failure("ocr_secondary_unread", error))?
            .is_some()
        {
            token = chat_timeline
                .request_refresh()
                .map_err(|error| after_input_failure("refresh_secondary_unread", error))?;
        }
        context
            .wait_cancellable(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(25)),
            )
            .map_err(|failure| {
                UiRoutineFailure::new(
                    InputCertainty::AfterInputUnknown,
                    "wait_secondary_unread_message",
                    failure.reason(),
                )
            })?;
    }
}

fn fresh_friend_snapshot(snapshot: &ChatTimelineSnapshot, opened_at: Instant) -> bool {
    snapshot.captured_at >= opened_at
        && matches!(snapshot.chat.as_ref(), Some(ChatIdentity::Friend(name)) if !name.trim().is_empty())
}

fn reacquire_unread_hit(
    context: &mut UiRoutineContext<'_>,
    config: &SecondaryUnreadRoutineConfig,
) -> Result<Option<UnreadFriendHit>, UiRoutineFailure> {
    let first = capture_normalized(
        context,
        &config.residency,
        "reacquire_secondary_unread",
        InputCertainty::AfterInputUnknown,
    )?;
    let Some(candidate) = find_unread_friend_hits(&first, &config.unread_layout)
        .into_iter()
        .next()
    else {
        return Ok(None);
    };
    wait_ms(
        context,
        bubble_poll_ms(config),
        "confirm_secondary_unread_hit",
        InputCertainty::AfterInputUnknown,
    )?;
    let confirmed = capture_normalized(
        context,
        &config.residency,
        "confirm_secondary_unread_hit",
        InputCertainty::AfterInputUnknown,
    )?;
    Ok(find_unread_friend_hits(&confirmed, &config.unread_layout)
        .into_iter()
        .find(|hit| same_unread_hit(&candidate, hit)))
}

fn same_unread_hit(first: &UnreadFriendHit, second: &UnreadFriendHit) -> bool {
    first.indicator.x.abs_diff(second.indicator.x) <= 4
        && first.indicator.y.abs_diff(second.indicator.y) <= 4
}

fn wait_bubble_stable(
    context: &mut UiRoutineContext<'_>,
    config: &SecondaryUnreadRoutineConfig,
    deadline: Instant,
) -> Result<(image::DynamicImage, Instant), UiRoutineFailure> {
    let mut previous = None;
    let mut stable_since = None;
    let poll_ms = bubble_poll_ms(config);
    let required_stable = Duration::from_millis(bounded_bubble_stability_ms(
        config.residency.friend_step_ms(),
        config.residency.timeout_ms(),
        poll_ms,
    ));
    while Instant::now() < deadline {
        let image = capture_normalized(
            context,
            &config.residency,
            "confirm_secondary_bubble",
            InputCertainty::AfterInputUnknown,
        )?;
        let current = latest_incoming_fingerprint(&image)
            .map_err(|error| after_input_failure("confirm_secondary_bubble", error))?;
        let captured_at = Instant::now();
        let unchanged = stable_bubble_sample(previous.as_ref(), current.as_ref());
        if stable_window_elapsed(unchanged, stable_since, captured_at, required_stable) {
            return Ok((image, captured_at));
        }
        if !unchanged {
            stable_since = current.as_ref().map(|_| captured_at);
        }
        previous = current;
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        wait_ms(
            context,
            next_bubble_sample_ms(poll_ms, stable_since, now, required_stable, deadline),
            "confirm_secondary_bubble",
            InputCertainty::AfterInputUnknown,
        )?;
    }
    Err(unread_message_failure(
        "incoming bubble did not appear and stabilize before timeout",
    ))
}

fn bounded_bubble_stability_ms(step_ms: u64, timeout_ms: u64, poll_ms: u64) -> u64 {
    step_ms.min(timeout_ms.saturating_sub(poll_ms).max(1))
}

fn next_bubble_sample_ms(
    poll_ms: u64,
    stable_since: Option<Instant>,
    now: Instant,
    required_stable: Duration,
    deadline: Instant,
) -> u64 {
    let deadline_ms = duration_ceil_ms(deadline.saturating_duration_since(now));
    let stability_ms = stable_since
        .map(|since| {
            duration_ceil_ms(required_stable.saturating_sub(now.saturating_duration_since(since)))
        })
        .unwrap_or(u64::MAX);
    poll_ms.min(deadline_ms).min(stability_ms).max(1)
}

fn duration_ceil_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis())
        .unwrap_or(u64::MAX)
        .saturating_add(u64::from(
            !duration.subsec_nanos().is_multiple_of(1_000_000),
        ))
}

fn bubble_poll_ms(config: &SecondaryUnreadRoutineConfig) -> u64 {
    config
        .bubble_poll_ms
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clamp(100, 200)
}

fn unread_message_failure(detail: impl Into<String>) -> UiRoutineFailure {
    UiRoutineFailure::new(
        InputCertainty::AfterInputUnknown,
        "wait_secondary_unread_message",
        detail,
    )
}

fn stable_bubble_sample(
    previous: Option<&crate::change_detection::ChangeFingerprint>,
    current: Option<&crate::change_detection::ChangeFingerprint>,
) -> bool {
    previous.is_some() && current.is_some() && !optional_fingerprint_changed(previous, current)
}

fn stable_window_elapsed(
    unchanged: bool,
    stable_since: Option<Instant>,
    now: Instant,
    required: Duration,
) -> bool {
    unchanged && stable_since.is_some_and(|since| now.saturating_duration_since(since) >= required)
}

fn unread_text_is_usable(text: &str) -> bool {
    !text.trim().is_empty()
}

fn optional_fingerprint_changed(
    previous: Option<&crate::change_detection::ChangeFingerprint>,
    current: Option<&crate::change_detection::ChangeFingerprint>,
) -> bool {
    match (previous, current) {
        (Some(previous), Some(current)) => {
            let stats = crate::change_detection::change_stats(previous, current);
            stats.mean_abs_diff >= 0.8 || stats.changed_ratio >= 0.01
        }
        (None, None) => true,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Rect;
    use crate::runtime::ocr::{OcrDevice, OcrLine, OcrRuntime};
    use crate::runtime::recovery::{GameRecoveryCoordinator, RecoveryTrigger};
    use crate::ui::routines::friend_delivery::FriendDeliveryRoutineConfig;
    use image::DynamicImage;

    #[test]
    fn unread_selection_promotes_latest_incoming_without_replaying_history() {
        use crate::observation::chat::RecognizedChatMessage;
        let shared = ChatTimeline::new();
        let listener = shared.subscribe().unwrap();
        let make = |text: &str, y, kind: &str| RecognizedChatMessage {
            text: text.into(),
            sender: None,
            raw_sender: None,
            message_type: kind.into(),
            block: Rect::new(10, y, 120, 20),
        };
        let chat = ChatIdentity::Friend(Arc::from("observer-confirmed friend"));
        let snapshot = shared
            .publish(
                Instant::now(),
                chat.clone(),
                vec![
                    make("old command", 10, "pink"),
                    make("new unread command", 40, "pink"),
                    make("my outgoing", 70, "blue"),
                ],
            )
            .unwrap();
        listener.read().unwrap();
        let selected = latest_unread_message(&snapshot).unwrap();
        assert_eq!(selected.text, "new unread command");
        assert!(
            shared
                .deliver_unread_baseline(&snapshot, &selected.id)
                .unwrap()
        );
        let delivery = listener.read().unwrap();
        assert_eq!(delivery[0].messages[0].id, selected.id);
        assert_eq!(delivery[0].messages.len(), 1);
        // 消费确认必须带快照时间：提升刚把围栏推到本次修订，同一帧的确认应当生效。
        assert!(
            shared
                .acknowledge_business_at(&selected.id, snapshot.captured_at)
                .unwrap()
        );
        assert!(
            !shared
                .deliver_unread_baseline(&snapshot, &selected.id)
                .unwrap()
        );

        let waiting = shared
            .publish(
                Instant::now(),
                chat,
                vec![make("old command", 10, "pink"), make(" ", 40, "pink")],
            )
            .unwrap();
        // Wait for the latest bubble's OCR; never substitute a historical command.
        assert!(latest_unread_message(&waiting).is_none());
    }

    #[test]
    fn initial_unread_clear_never_promotes_a_command() {
        // 初始红点清场（进入二级值守那一次）只建立基线、清红点；即使屏幕上就有命令气泡，
        // 也不能被提升成一次派发。清场期间 UI 事务持有好友业务抑制，共享观察会把看到的
        // 气泡直接永久 ACK，因此这里既进不了未读基线，也不会变成待派发。
        use crate::observation::chat::RecognizedChatMessage;
        let shared = ChatTimeline::new();
        let listener = shared.subscribe().unwrap();
        let command = |text: &str, y| RecognizedChatMessage {
            text: text.into(),
            sender: None,
            raw_sender: None,
            message_type: "pink".into(),
            block: Rect::new(10, y, 120, 20),
        };
        let chat = ChatIdentity::Friend(Arc::from("clear-baseline friend"));
        let _suppression = shared.suppress_friend_business().unwrap();
        let snapshot = shared
            .publish(Instant::now(), chat, vec![command("@状态", 40)])
            .unwrap();
        listener.read().unwrap();
        let visible = &snapshot.messages[0];
        // 气泡在画面上可见（因此"最后一条命令"的候选判定本身成立）……
        assert!(latest_unread_message(&snapshot).is_some());
        // ……但清场不允许把它提升，也不存在任何待派发状态。
        assert!(
            !shared
                .deliver_unread_baseline(&snapshot, &visible.id)
                .unwrap()
        );
        assert!(
            !shared
                .is_business_pending_at(&visible.id, snapshot.captured_at)
                .unwrap()
        );
        // 抑制期间共享观察已把它永久 ACK；本帧时间不早于围栏，所以这里如实返回 false。
        assert!(
            !shared
                .acknowledge_business_at(&visible.id, snapshot.captured_at)
                .unwrap()
        );
    }

    #[test]
    fn shared_friend_snapshot_requires_fresh_observer_identity() {
        let opened_at = Instant::now();
        let mut snapshot = ChatTimelineSnapshot {
            visual_session: crate::observation::chat::VisualSessionId::new(1),
            captured_at: opened_at - Duration::from_millis(1),
            scope: crate::observation::chat::ChatScope::private(Arc::from("Alice")),
            source: crate::observation::chat::SourceId::ocr(),
            chat: Some(ChatIdentity::Friend(Arc::from("Alice"))),
            messages: Vec::new(),
        };
        assert!(!fresh_friend_snapshot(&snapshot, opened_at));
        snapshot.captured_at = opened_at;
        assert!(fresh_friend_snapshot(&snapshot, opened_at));
        for chat in [
            ChatIdentity::Unknown,
            ChatIdentity::SecondaryHall,
            ChatIdentity::Friend(Arc::from("  ")),
        ] {
            snapshot.chat = Some(chat);
            assert!(!fresh_friend_snapshot(&snapshot, opened_at));
        }
    }
    use crate::change_detection::ChangeFingerprint;
    use crate::geometry::Point;

    fn fingerprint(value: u8) -> ChangeFingerprint {
        ChangeFingerprint {
            pixels: vec![value; 16],
            width: 4,
            height: 4,
        }
    }

    #[test]
    fn empty_bubble_frames_are_never_stable() {
        assert!(!stable_bubble_sample(None, None));
        assert!(optional_fingerprint_changed(None, None));
    }

    #[test]
    fn bubble_requires_two_present_matching_samples() {
        let first = fingerprint(20);
        let same = fingerprint(20);
        let changed = fingerprint(220);

        assert!(!stable_bubble_sample(None, Some(&first)));
        assert!(stable_bubble_sample(Some(&first), Some(&same)));
        assert!(!stable_bubble_sample(Some(&first), Some(&changed)));
    }

    #[test]
    fn bubble_must_remain_unchanged_for_the_settle_window() {
        let started = Instant::now();
        let required = Duration::from_millis(800);

        assert!(!stable_window_elapsed(
            true,
            Some(started),
            started + Duration::from_millis(799),
            required,
        ));
        assert!(stable_window_elapsed(
            true,
            Some(started),
            started + required,
            required,
        ));
        assert!(!stable_window_elapsed(
            false,
            Some(started),
            started + required,
            required,
        ));
    }

    #[test]
    fn bubble_stability_window_leaves_room_for_a_confirmation_sample() {
        assert_eq!(bounded_bubble_stability_ms(800, 5_000, 200), 800);
        assert_eq!(bounded_bubble_stability_ms(5_000, 5_000, 200), 4_800);
        assert_eq!(bounded_bubble_stability_ms(8_000, 5_000, 200), 4_800);

        let started = Instant::now();
        assert_eq!(
            next_bubble_sample_ms(
                200,
                Some(started),
                started + Duration::from_millis(799),
                Duration::from_millis(800),
                started + Duration::from_secs(5),
            ),
            1
        );
    }

    #[test]
    fn unread_message_text_must_not_be_blank() {
        assert!(!unread_text_is_usable("  \r\n "));
        assert!(unread_text_is_usable("@帮助"));
    }

    #[test]
    fn fresh_unread_hit_confirmation_allows_small_detection_drift() {
        let first = UnreadFriendHit {
            indicator: Point::new(64, 310),
            row_click: Point::new(150, 310),
        };
        let close = UnreadFriendHit {
            indicator: Point::new(67, 307),
            row_click: Point::new(150, 307),
        };
        let moved = UnreadFriendHit {
            indicator: Point::new(64, 330),
            row_click: Point::new(150, 330),
        };

        assert!(same_unread_hit(&first, &close));
        assert!(!same_unread_hit(&first, &moved));
    }

    /// P1 最终收口：未读任务的价值基线是“当前大厅二级”，
    /// 收尾固定在这里且只由包装器 Fixed 的唯一引擎执行一次。
    #[test]
    fn secondary_unread_task_spec_is_fixed_to_current_hall() {
        let config = crate::config::bundled_config();
        let spec = process_secondary_unread_task_spec(SettleEnvironment::without_ocr(
            FriendDeliveryRoutineConfig::from_app(&config),
        ));
        assert_eq!(spec.entry, EntryPolicy::Passthrough);
        assert_eq!(
            spec.finish,
            FinishPolicy::Fixed(ResidencyTarget::Secondary(ConversationRef::CurrentHall)),
            "未读任务必须用会话级 Fixed 收尾，且只由唯一引擎执行一次"
        );
        assert_eq!(
            spec.target,
            ResidencyTarget::Secondary(ConversationRef::CurrentHall)
        );
    }

    struct EmptySecondaryUnreadOcr;

    impl OcrDevice for EmptySecondaryUnreadOcr {
        fn recognize_lines(&mut self, _image: &DynamicImage) -> anyhow::Result<Vec<OcrLine>> {
            Ok(Vec::new())
        }
    }

    fn unread_request() -> ProcessSecondaryUnread {
        ProcessSecondaryUnread::new(
            UnreadFriendHit {
                indicator: crate::geometry::Point::new(67, 307),
                row_click: crate::geometry::Point::new(150, 307),
            },
            true,
        )
    }

    fn unread_config() -> SecondaryUnreadRoutineConfig {
        SecondaryUnreadRoutineConfig::resolve(
            FriendDeliveryRoutineConfig::from_app(&crate::config::bundled_config()),
            0,
            Arc::new(RwLock::new(100)),
        )
    }

    /// B15-P2-4 回归：未读任务的两个环境构造点都必须共享进程级协调器。
    ///
    /// 一处是 `UiTask` 的收尾环境，另一处是例程开头「当前大厅」的业务基线；
    /// 少接一处，那一次恢复采样就记进私有实例。
    #[test]
    fn secondary_unread_paths_share_the_injected_recovery_coordinator() {
        let ocr_runtime = OcrRuntime::start(EmptySecondaryUnreadOcr, 1).unwrap();
        let shared = GameRecoveryCoordinator::default();
        shared.record_restart_completed(
            std::time::Instant::now(),
            RecoveryTrigger::ExternalChatDisconnect,
        );
        let timeline = ChatTimeline::new();

        let task = ProcessSecondaryUnreadTask {
            request: unread_request(),
            ocr: ocr_runtime.handle(),
            chat_timeline: timeline.clone(),
            config: unread_config(),
            recovery: shared.clone(),
        };
        assert_eq!(
            task.environment()
                .recovery()
                .health_snapshot_now()
                .total_restarts,
            1,
            "未读任务的收尾环境必须共享注入的协调器"
        );

        let routine = ProcessSecondaryUnreadRoutine {
            request: unread_request(),
            ocr: ocr_runtime.handle(),
            chat_timeline: timeline,
            config: unread_config(),
            recovery: shared.clone(),
        };
        assert_eq!(
            routine
                .environment()
                .recovery()
                .health_snapshot_now()
                .total_restarts,
            1,
            "业务前置基线也必须共享注入的协调器"
        );

        let unwired = ProcessSecondaryUnreadRoutine {
            request: unread_request(),
            ocr: ocr_runtime.handle(),
            chat_timeline: ChatTimeline::new(),
            config: unread_config(),
            recovery: GameRecoveryCoordinator::default(),
        };
        assert_eq!(
            unwired
                .environment()
                .recovery()
                .health_snapshot_now()
                .total_restarts,
            0,
            "默认构造的协调器不得看到共享实例的重启记录"
        );
        ocr_runtime.shutdown().unwrap();
    }
}
