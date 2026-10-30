use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use super::friend_delivery::{
    FriendDeliveryRoutineConfig, UiResidencyOutcome, UiResidencyTarget, after_input_failure,
    before_input_failure, capture_normalized, restore_residency, wait_ms,
};
use crate::observation::chat::{
    ChatIdentity, ChatTimeline, ChatTimelineSnapshot, FriendUnreadLayout, ObservedChatMessageId,
    UnreadFriendHit, find_unread_friend_hits, latest_incoming_fingerprint,
    unread_hit_still_visible,
};
use crate::runtime::ocr::OcrRuntimeHandle;
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
#[allow(dead_code)]
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
        }
    }

    pub(crate) fn submit(
        &self,
        request: ProcessSecondaryUnread,
    ) -> Result<UiOperation<ProcessSecondaryUnreadOutcome>, UiSubmitError> {
        self.runtime.submit(ProcessSecondaryUnreadRoutine {
            request,
            ocr: self.ocr.clone(),
            chat_timeline: self.chat_timeline.clone(),
            config: self.config.clone(),
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
}

impl sealed::UiRoutineSealed for ProcessSecondaryUnreadRoutine {}

impl UiRoutine for ProcessSecondaryUnreadRoutine {
    type Output = ProcessSecondaryUnreadOutcome;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        // Acquire before any navigation and keep suppression through restoration,
        // including failure/cancellation paths. No service lock spans UI waits.
        let _discard_guard = if self.request.discard_only {
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
        let effect = match process_unread(
            context,
            &self.request,
            &self.ocr,
            &self.chat_timeline,
            &self.config,
        ) {
            Ok(effect) => effect,
            Err(failure) => SecondaryUnreadEffect::Failed(failure),
        };
        let residency = match restore_residency(
            context,
            &self.ocr,
            &self.config.residency,
            UiResidencyTarget::SecondaryCurrentHall,
        ) {
            Ok(()) => UiResidencyOutcome::Confirmed(UiResidencyTarget::SecondaryCurrentHall),
            Err(failure) => UiResidencyOutcome::Failed(failure),
        };
        ProcessSecondaryUnreadOutcome { effect, residency }
    }
}

fn process_unread(
    context: &mut UiRoutineContext<'_>,
    request: &ProcessSecondaryUnread,
    ocr: &OcrRuntimeHandle,
    chat_timeline: &ChatTimeline,
    config: &SecondaryUnreadRoutineConfig,
) -> Result<SecondaryUnreadEffect, UiRoutineFailure> {
    context
        .device()
        .ensure_ready(config.residency.after_activate_ms)
        .map_err(|error| before_input_failure("prepare_secondary_unread", error))?;
    restore_residency(
        context,
        ocr,
        &config.residency,
        UiResidencyTarget::SecondaryCurrentHall,
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
            for message in &snapshot.messages {
                chat_timeline
                    .acknowledge_business(&message.id)
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
                log::warn!(
                    "二级好友未读提升未生效（步骤：提升未读消息）：好友「{}」的这条气泡不在首屏基线内，无法据此派发命令；该气泡可能已按常规路径交付过（{}）",
                    friend_name,
                    if chat_timeline
                        .is_business_pending(&message.id)
                        .unwrap_or(false)
                    {
                        "当前仍在待处理队列中"
                    } else {
                        "当前不在待处理队列中"
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
            .is_business_pending(id)
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
        assert!(shared.acknowledge_business(&selected.id).unwrap());
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
        assert!(!shared.is_business_pending(&visible.id).unwrap());
        assert!(!shared.acknowledge_business(&visible.id).unwrap());
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
}
