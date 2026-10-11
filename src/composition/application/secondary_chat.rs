use super::*;

struct SecondaryUnreadTaskCompletionGuard {
    business: BusinessRuntimeHandle,
    active: bool,
    processed_message: bool,
}

impl SecondaryUnreadTaskCompletionGuard {
    fn new(business: BusinessRuntimeHandle) -> Self {
        Self {
            business,
            active: true,
            processed_message: false,
        }
    }

    fn mark_message_processed(&mut self) {
        self.processed_message = true;
    }

    fn finish(mut self) -> Result<()> {
        self.business
            .finish_chat_listener_unread_task(self.processed_message)?;
        self.active = false;
        Ok(())
    }
}

impl Drop for SecondaryUnreadTaskCompletionGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let release = if self.processed_message {
            self.business.finish_chat_listener_unread_task(true)
        } else {
            self.business.release_chat_listener_unread_task()
        };
        if let Err(error) = release {
            log::error!("二级未读任务异常结束后释放 pending 状态失败: {error}");
        } else {
            log::warn!("二级未读任务异常结束，已释放 pending 状态并等待下一轮恢复");
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct SecondaryObservationOutcome {
    pub(super) processed: bool,
    pub(super) command_dispatched: bool,
    pub(super) unrecognized: bool,
}

#[derive(Clone, Debug)]
struct SecondaryOcrMessage {
    text: String,
    sender: Option<String>,
    sender_identity: Option<SecondarySenderIdentity>,
    kind: SecondaryHallMessageKind,
    requires_sender: bool,
}

fn classify_secondary_hall_routed_message(
    text: &str,
    routed: Option<&RoutedCommand>,
    accepts_turtle_questions: bool,
    commands_enabled: bool,
) -> SecondaryHallMessageClassification {
    let mut classification = classify_secondary_hall_message(
        text,
        routed.map(|command| &command.command),
        accepts_turtle_questions,
    );
    // 第一条路由使用占位发言者，因此授权判断需要真实发言者。
    classification.requires_sender |= routed
        .is_some_and(|command| command.permission_required.is_some())
        || (!commands_enabled && classification.kind == SecondaryHallMessageKind::Command);
    classification
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SecondaryHallActorObservation {
    nickname: String,
    remark: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SecondaryHallCommandObservation {
    command_key: String,
    actor: Option<SecondaryHallActorObservation>,
    requires_sender: bool,
    row_from_bottom: usize,
}

impl SecondaryHallCommandObservation {
    fn stable_with(&self, other: &Self) -> bool {
        if self.command_key != other.command_key || self.requires_sender != other.requires_sender {
            return false;
        }
        if !self.requires_sender {
            return true;
        }
        if self.actor.is_none() || other.actor.is_none() {
            return self.actor.is_none() && other.actor.is_none();
        }
        self.matches(other)
    }

    fn matches(&self, other: &Self) -> bool {
        if self.command_key != other.command_key || self.requires_sender != other.requires_sender {
            return false;
        }
        if !self.requires_sender {
            return true;
        }
        let (Some(left), Some(right)) = (&self.actor, &other.actor) else {
            return false;
        };
        let left_effective = left.remark.as_deref().unwrap_or(&left.nickname);
        let right_effective = right.remark.as_deref().unwrap_or(&right.nickname);
        left_effective == right_effective
            || (left.nickname == right.nickname
                && (left.remark.is_none() || right.remark.is_none() || left.remark == right.remark))
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct SecondaryHallCommandTracker {
    visible: Vec<SecondaryHallCommandObservation>,
    initialized: bool,
    last_scan_at: Option<Instant>,
    visible_bubble_count: usize,
}

impl SecondaryHallCommandTracker {
    pub(super) fn reset(&mut self) {
        self.visible.clear();
        self.initialized = false;
        self.last_scan_at = None;
        self.visible_bubble_count = 0;
    }

    fn scan_due(&self, now: Instant, fallback_ms: u64) -> bool {
        self.last_scan_at.is_none_or(|last| {
            now.saturating_duration_since(last) >= Duration::from_millis(fallback_ms.max(1))
        })
    }

    fn begin_scan(&mut self, now: Instant) {
        self.last_scan_at = Some(now);
    }

    fn snapshot(messages: &[SecondaryOcrMessage]) -> Vec<SecondaryHallCommandObservation> {
        messages
            .iter()
            .enumerate()
            .filter_map(|(index, message)| {
                secondary_hall_command_observation(message, messages.len() - 1 - index)
            })
            .collect()
    }

    fn snapshots_match(previous: &[SecondaryOcrMessage], current: &[SecondaryOcrMessage]) -> bool {
        let previous = Self::snapshot(previous);
        let current = Self::snapshot(current);
        previous.len() == current.len()
            && previous
                .iter()
                .zip(current.iter())
                .all(|(left, right)| left.stable_with(right))
    }

    fn preview_new_indexes(&self, messages: &[SecondaryOcrMessage]) -> Vec<usize> {
        if !self.initialized {
            return Vec::new();
        }

        let mut matched = vec![false; self.visible.len()];
        let mut new_indexes = Vec::new();
        for (index, message) in messages.iter().enumerate() {
            let Some(current) =
                secondary_hall_command_observation(message, messages.len() - 1 - index)
            else {
                continue;
            };
            let candidate = self
                .visible
                .iter()
                .enumerate()
                .filter(|(previous_index, previous)| {
                    !matched[*previous_index] && previous.matches(&current)
                })
                .min_by_key(|(_, previous)| {
                    previous.row_from_bottom.abs_diff(current.row_from_bottom)
                })
                .map(|(previous_index, _)| previous_index);
            if let Some(previous_index) = candidate {
                matched[previous_index] = true;
            } else {
                new_indexes.push(index);
            }
        }
        new_indexes
    }

    fn commit(&mut self, messages: &[SecondaryOcrMessage]) {
        self.visible = Self::snapshot(messages);
        self.initialized = true;
        self.visible_bubble_count = messages.len();
    }

    fn should_hold_incomplete(&self, messages: &[SecondaryOcrMessage], same_layout: bool) -> bool {
        self.initialized
            && same_layout
            && messages.len() == self.visible_bubble_count
            && Self::snapshot(messages).len() < self.visible.len()
    }
}

fn secondary_hall_command_observation(
    message: &SecondaryOcrMessage,
    row_from_bottom: usize,
) -> Option<SecondaryHallCommandObservation> {
    let command_key = if let Some(command) = secondary_hall_command_text(&message.text) {
        let command = command.trim();
        let (prefix, payload) = if let Some(payload) = command
            .strip_prefix('@')
            .or_else(|| command.strip_prefix('＠'))
        {
            ('@', payload)
        } else {
            (
                '#',
                command
                    .strip_prefix('#')
                    .or_else(|| command.strip_prefix('＃'))?,
            )
        };
        format!("{prefix}:{}", command_identity(payload))
    } else {
        let input = crate::features::song_request::parse_structured_song_text(&message.text)?;
        format!("@:{}", command_identity(&input.keyword()))
    };
    let actor = if message.requires_sender {
        message
            .sender_identity
            .as_ref()
            .map(|identity| SecondaryHallActorObservation {
                nickname: command_identity(&identity.nickname),
                remark: identity
                    .remark
                    .as_deref()
                    .map(command_identity)
                    .filter(|remark| !remark.is_empty()),
            })
            .or_else(|| {
                message
                    .sender
                    .as_deref()
                    .filter(|sender| !sender.trim().is_empty())
                    .map(|sender| SecondaryHallActorObservation {
                        nickname: command_identity(sender),
                        remark: None,
                    })
            })
    } else {
        None
    };
    Some(SecondaryHallCommandObservation {
        command_key,
        actor,
        requires_sender: message.requires_sender,
        row_from_bottom,
    })
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct SecondaryListenerRoundOutcome {
    pub(super) scanned: bool,
    pub(super) verified: bool,
    pub(super) recovery_requested: bool,
}

pub(super) struct SecondaryListenerRoundState<'a> {
    pub(super) last_friend_bubble: &'a mut Option<ChangeFingerprint>,
    pub(super) hall_bubble_sequence: &'a mut Option<Vec<SecondaryHallBubble>>,
    pub(super) hall_command_tracker: &'a mut SecondaryHallCommandTracker,
    pub(super) identity: &'a mut Option<SecondaryChatIdentity>,
}

impl ApplicationRuntime {
    pub(super) fn run_secondary_listener_round(
        &mut self,
        image: &DynamicImage,
        state: SecondaryListenerRoundState<'_>,
        allow_hall_recovery: bool,
    ) -> Result<SecondaryListenerRoundOutcome> {
        let SecondaryListenerRoundState {
            last_friend_bubble,
            hall_bubble_sequence,
            hall_command_tracker,
            identity,
        } = state;
        if !self.business.task_engine.is_idle()? {
            return Ok(SecondaryListenerRoundOutcome::default());
        }
        let unread_layout = crate::observation::chat::FriendUnreadLayout::resolve(
            self.lifecycle.config.screen.expected_width,
            self.lifecycle.config.screen.expected_height,
            self.lifecycle.config.invite.friend_list_region.into(),
        );
        let shared_snapshot = self.ui.shared_ocr.latest()?;
        let observed_identity = if shared_snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.chat == crate::observation::chat::ChatIdentity::SecondaryHall
        }) {
            SecondaryChatIdentity::CurrentHall
        } else {
            SecondaryChatIdentity::Unknown
        };
        let scanned = shared_snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.chat == crate::observation::chat::ChatIdentity::SecondaryHall
        });
        let verified = observed_identity == SecondaryChatIdentity::CurrentHall;
        let identity_changed = identity.as_ref() != Some(&observed_identity);
        if identity_changed {
            hall_bubble_sequence.take();
            hall_command_tracker.reset();
        }
        *identity = Some(observed_identity);
        let state = self.business.business.chat_listener_snapshot()?;
        if state.unread_task_pending {
            return Ok(SecondaryListenerRoundOutcome::default());
        }
        let current_identity = identity.clone().unwrap_or(SecondaryChatIdentity::Unknown);

        if state.initial_unread_clear {
            // 初始红点清场：只建立基线并清红点，历史一律不执行。因此清场期间排出的
            // 未读任务一律 discard_only=true；只有清场结束、之后真正进入带新未读的
            // 会话时才排 discard_only=false —— 那时才读取最后发送的那条命令并派发一次。
            if current_identity != SecondaryChatIdentity::CurrentHall {
                let recovery_requested =
                    allow_hall_recovery && self.queue_secondary_hall_recovery()?;
                return Ok(SecondaryListenerRoundOutcome {
                    recovery_requested,
                    ..Default::default()
                });
            }
            if let Some(hit) = find_unread_friend_hits(image, &unread_layout)
                .into_iter()
                .next()
            {
                self.queue_secondary_unread_task(hit, true)?;
                return Ok(SecondaryListenerRoundOutcome::default());
            }
            *last_friend_bubble = latest_incoming_fingerprint(image)?;
            self.business
                .business
                .finish_chat_listener_initial_unread_clear()?;
            log::info!("二级监听初始未读清场完成，当前大厅已建立消息基线");
            return Ok(SecondaryListenerRoundOutcome {
                scanned: false,
                verified: true,
                ..Default::default()
            });
        }

        match current_identity {
            SecondaryChatIdentity::CurrentHall => {
                if state.hall_round_required {
                    self.business.business.finish_chat_listener_hall_round()?;
                    return Ok(SecondaryListenerRoundOutcome {
                        scanned,
                        verified,
                        ..Default::default()
                    });
                }
                if let Some(hit) = find_unread_friend_hits(image, &unread_layout)
                    .into_iter()
                    .next()
                {
                    self.queue_secondary_unread_task(hit, false)?;
                    return Ok(SecondaryListenerRoundOutcome::default());
                }
                Ok(SecondaryListenerRoundOutcome {
                    scanned,
                    verified,
                    ..Default::default()
                })
            }
            SecondaryChatIdentity::Unknown => {
                let recovery_requested =
                    allow_hall_recovery && self.queue_secondary_hall_recovery()?;
                Ok(SecondaryListenerRoundOutcome {
                    recovery_requested,
                    ..Default::default()
                })
            }
        }
    }

    fn queue_secondary_unread_task(
        &self,
        hit: UnreadFriendHit,
        discard_only: bool,
    ) -> Result<bool> {
        if !self.business.business.claim_chat_listener_unread_task()? {
            return Ok(false);
        }
        let queued_y = hit.row_click.y;
        if let Err(error) =
            self.push_pending_task(PendingTask::SecondaryUnread { hit, discard_only })
        {
            self.business.business.release_chat_listener_unread_task()?;
            return Err(error);
        }
        log::info!(
            "二级监听检测到好友未读红点: y={} discard_only={}",
            queued_y,
            discard_only
        );
        Ok(false)
    }

    fn queue_secondary_hall_recovery(&self) -> Result<bool> {
        if !self.business.business.claim_chat_listener_unread_task()? {
            return Ok(false);
        }
        if let Err(error) = self.push_pending_task(PendingTask::RestoreSecondaryHall) {
            self.business.business.release_chat_listener_unread_task()?;
            return Err(error);
        }
        log::info!("二级监听检测到不可执行会话，已加入恢复当前大厅任务");
        Ok(true)
    }

    pub(super) fn execute_secondary_unread_task(
        &mut self,
        hit: UnreadFriendHit,
        discard_only: bool,
    ) -> Result<()> {
        let mut completion =
            SecondaryUnreadTaskCompletionGuard::new(self.business.business.clone());
        let outcome = self
            .ui
            .secondary_unread_ui
            .submit(ProcessSecondaryUnread::new(hit, discard_only))
            .context("提交二级好友未读 UI 事务")?
            .wait()
            .context("等待二级好友未读 UI 事务")?;
        let processed_message = match outcome.effect() {
            SecondaryUnreadEffect::Message { .. } => {
                // The UI transaction waits for fresh shared OCR before restoring the hall.
                // The retained business subscriber owns dispatch; this task only releases its claim.
                if !discard_only {
                    completion.mark_message_processed();
                }
                !discard_only
            }
            SecondaryUnreadEffect::Discarded
            | SecondaryUnreadEffect::NoMessage
            | SecondaryUnreadEffect::Stale => false,
            SecondaryUnreadEffect::Failed(failure) => {
                return Err(anyhow!("二级好友未读处理失败：{failure}"));
            }
        };
        match outcome.residency() {
            UiResidencyOutcome::Confirmed(UiResidencyTarget::SecondaryCurrentHall) => {}
            UiResidencyOutcome::Confirmed(other) => {
                return Err(anyhow!("二级好友未读驻留结果错误: {other:?}"));
            }
            UiResidencyOutcome::Failed(failure) => {
                return Err(anyhow!("二级好友未读后恢复当前大厅失败：{failure}"));
            }
        }

        debug_assert_eq!(processed_message, completion.processed_message);
        completion.finish()
    }

    pub(super) fn execute_restore_secondary_hall_task(&mut self) -> Result<()> {
        let result = self.establish_ui_residency(
            UiResidency::SecondaryCurrentHall,
            ResidencyPurpose::IndependentRecovery("恢复二级当前大厅任务"),
        );
        self.business
            .business
            .finish_chat_listener_unread_task(false)?;
        result.context("二级监听暂时无法恢复当前大厅，已保留二级模式并等待重试")
    }

    pub(super) fn begin_chat_decision_reader<A, P>(
        &self,
        scope: ChatDecisionScope,
        accepts_message_type: &A,
        is_decision: &P,
    ) -> Result<ChatDecisionReader>
    where
        A: Fn(&str) -> bool,
        P: Fn(&str) -> bool,
    {
        let deadline = Instant::now()
            + Duration::from_millis(self.lifecycle.config.timing.decision.timeout_ms);
        self.begin_chat_decision_reader_until(scope, accepts_message_type, is_decision, deadline)
    }

    /// 调用方给出整个交互的截止时间，基线采集也计入总期限。
    pub(super) fn begin_chat_decision_reader_until<A, P>(
        &self,
        scope: ChatDecisionScope,
        accepts_message_type: &A,
        is_decision: &P,
        deadline: Instant,
    ) -> Result<ChatDecisionReader>
    where
        A: Fn(&str) -> bool,
        P: Fn(&str) -> bool,
    {
        let observation_session =
            self.ui
                .chat_observations
                .begin_exclusive_until(deadline, || {
                    !self.lifecycle.running.load(AtomicOrdering::SeqCst)
                        || self.lifecycle.paused.load(AtomicOrdering::SeqCst)
                        || self.lifecycle.live_configs.has_pending_reload()
                })?;
        let listener_mode = self.business.business.chat_listener_snapshot()?.mode;
        let chat = if scope == ChatDecisionScope::CurrentHall
            && self.active_ui_residency()? == UiResidency::SecondaryCurrentHall
        {
            crate::observation::chat::ChatIdentity::SecondaryHall
        } else {
            crate::observation::chat::ChatIdentity::PrimaryHall
        };
        let subscriber = self.ui.shared_ocr.subscribe()?;
        let mut refresh = self.ui.shared_ocr.request_refresh()?;
        let baseline = loop {
            if !self.lifecycle.running.load(AtomicOrdering::SeqCst) {
                return Err(anyhow!("运行已停止，已结束确认窗口"));
            }
            if self.lifecycle.paused.load(AtomicOrdering::SeqCst) {
                return Err(anyhow!("运行已暂停，已结束确认窗口"));
            }
            if self.lifecycle.live_configs.has_pending_reload() {
                return Err(anyhow!("配置重载待处理，已结束确认窗口"));
            }
            let mode = self.business.business.chat_listener_snapshot()?;
            if mode.mode != listener_mode || mode.pending_mode.is_some() {
                return Err(anyhow!("监听模式正在切换，已结束确认窗口"));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(anyhow!("等待共享聊天确认基线超时"));
            }
            let refreshed = self
                .ui
                .shared_ocr
                .wait_for_snapshot(refresh, remaining.min(Duration::from_millis(50)));
            let snapshot = match refreshed {
                Ok(Some(snapshot)) => snapshot,
                Ok(None) => continue,
                Err(error)
                    if error
                        .downcast_ref::<crate::observation::chat::NoChatRefreshError>()
                        .is_some() =>
                {
                    refresh = self.ui.shared_ocr.request_refresh()?;
                    continue;
                }
                Err(error) => return Err(error),
            };
            // 一级大厅下发消息必然经过一次二级面板：那是同一大厅的另一块面板，
            // 属于预期内的瞬时状态，不得用它做基线游标，也不得重用已经完成的刷新 token。
            if let Some(next) = self
                .ui
                .shared_ocr
                .refresh_after_other_hall_panel(&chat, &snapshot)?
            {
                refresh = next;
                continue;
            }
            if snapshot.chat != chat {
                // A task may temporarily expose another chat for several seconds.
                // Keep capturing until its bound chat returns or the outer deadline expires.
                refresh = self.ui.shared_ocr.request_refresh()?;
                continue;
            }
            break snapshot;
        };
        if baseline.chat != chat {
            return Err(anyhow!("共享聊天确认基线会话不匹配"));
        }
        subscriber.bind_chat(chat.clone(), baseline.visual_session)?;
        let baseline_cursor = Some((
            baseline.visual_session,
            baseline
                .messages
                .iter()
                .map(|message| message.id.bubble_sequence.get())
                .max()
                .unwrap_or(0),
        ));
        let messages = baseline
            .messages
            .iter()
            .map(|message| ChatMessage {
                message_type: message.message_type.clone(),
                block: message.block,
                text: message.text.clone(),
            })
            .collect::<Vec<_>>();
        Ok(ChatDecisionReader {
            listener_mode,
            subscriber,
            chat,
            baseline_at: baseline.captured_at,
            baseline_cursor,
            seen: baseline
                .messages
                .into_iter()
                .map(|message| (message.id, message.text))
                .collect(),
            screen_lock: DecisionScreenLock::from_messages(
                &messages,
                accepts_message_type,
                is_decision,
            ),
            _observation_session: observation_session,
        })
    }

    pub(super) fn wait_for_chat_decision_messages(
        &self,
        reader: &mut ChatDecisionReader,
        configured_ms: u64,
        remaining: Duration,
    ) -> Result<Vec<ChatMessage>> {
        let wait_until = Instant::now() + remaining;
        let result = (|| {
            if !self.lifecycle.running.load(AtomicOrdering::SeqCst) {
                return Ok(Vec::new());
            }
            let messages = self.poll_chat_decision_reader(reader)?;
            if !messages.is_empty() {
                return Ok(messages);
            }
            reader.wait_for_input(
                configured_ms,
                wait_until.saturating_duration_since(Instant::now()),
            )?;
            if !self.lifecycle.running.load(AtomicOrdering::SeqCst) {
                return Ok(Vec::new());
            }
            self.poll_chat_decision_reader(reader)
        })();
        if !self.lifecycle.running.load(AtomicOrdering::SeqCst) {
            return Ok(Vec::new());
        }
        if let Err(error) = &result {
            log::error!("确认窗口读取失败: {error:#}");
            if self.lifecycle.running.load(AtomicOrdering::SeqCst) {
                if let Err(reply_error) =
                    self.reply("确认窗口已结束：无法继续读取聊天，请稍后重试。")
                {
                    log::warn!("确认窗口失败反馈发送失败: {reply_error:#}");
                }
            }
        }
        result
    }

    pub(super) fn poll_chat_decision_reader(
        &self,
        reader: &mut ChatDecisionReader,
    ) -> Result<Vec<ChatMessage>> {
        if self.lifecycle.paused.load(AtomicOrdering::SeqCst) {
            return Err(anyhow!("运行已暂停，已结束确认窗口"));
        }
        if self.lifecycle.live_configs.has_pending_reload() {
            return Err(anyhow!("配置重载待处理，已结束确认窗口"));
        }
        let snapshot = self.business.business.chat_listener_snapshot()?;
        if snapshot.mode != reader.listener_mode || snapshot.pending_mode.is_some() {
            return Err(anyhow!("监听模式正在切换，已结束确认窗口"));
        }
        reader.read_messages()
    }

    pub(super) fn classify_secondary_business_message(
        &self,
        text: &str,
        accepts_turtle_questions: bool,
    ) -> Result<SecondaryHallMessageClassification> {
        let router = ChatCommandRouter::with_identity(
            &self.business.custom_workflow,
            &self.lifecycle.live_configs.identity,
        );
        let active_entertainment = self.business.business.active_entertainment()?;
        let routed = secondary_hall_command_text(text)
            .and_then(|command_text| {
                CommandEnvelope::new(
                    text,
                    SECONDARY_HALL_FALLBACK_SENDER,
                    "blue",
                    command_text,
                    CommandObservation::default(),
                )
            })
            .or_else(|| {
                command::parse_structured_song_envelope(
                    text,
                    SECONDARY_HALL_FALLBACK_SENDER,
                    "blue",
                    CommandObservation::default(),
                )
            })
            .and_then(|envelope| router.route(&envelope, active_entertainment));
        Ok(classify_secondary_hall_routed_message(
            text,
            routed.as_ref(),
            accepts_turtle_questions,
            self.commands_enabled()?,
        ))
    }

    pub(super) fn process_secondary_chat_observation(
        &self,
        frame: ObservedFrame,
        observation: SecondaryChatObservation,
    ) -> Result<SecondaryObservationOutcome> {
        let SecondaryChatObservation {
            message_type,
            friend_name,
            accepts_turtle_questions,
            messages,
        } = observation;
        let mut outcome = SecondaryObservationOutcome::default();
        for SecondaryObservedMessage {
            id: message_id,
            text,
            sender: message_sender,
        } in messages
        {
            log::debug!("处理二级观察消息: id={message_id:?}");
            let command_observation = CommandObservation {
                frame_id: Some(frame.id()),
                captured_at: Some(frame.captured_at()),
                message_id: Some(message_id.clone()),
                message_text: Some(text.clone()),
                message_y: None,
            };
            let shortcut_player = if message_type == "pink" {
                friend_name.trim().to_string()
            } else {
                message_sender
                    .as_deref()
                    .map(str::trim)
                    .filter(|sender| !sender.is_empty())
                    .unwrap_or(SECONDARY_HALL_FALLBACK_SENDER)
                    .to_string()
            };
            let router = ChatCommandRouter::with_identity(
                &self.business.custom_workflow,
                &self.lifecycle.live_configs.identity,
            );
            if message_type == "blue" {
                let classification =
                    self.classify_secondary_business_message(&text, accepts_turtle_questions)?;
                if classification.kind == SecondaryHallMessageKind::Ignored
                    || (classification.requires_sender
                        && message_sender
                            .as_deref()
                            .is_none_or(|sender| sender.trim().is_empty()))
                {
                    log::debug!("二级身份相关输入尚未取得发送者，保留共享消息等待身份修订");
                    outcome.unrecognized = true;
                    continue;
                }
            }
            if let Some(envelope) = command::parse_structured_song_envelope(
                &text,
                &shortcut_player,
                &message_type,
                command_observation.clone(),
            ) && let Some(parsed) =
                router.route(&envelope, self.business.business.active_entertainment()?)
            {
                outcome.processed = true;
                outcome.command_dispatched |= self.submit_secondary_command(parsed)?;
                continue;
            }
            if let Some(envelope) = CommandEnvelope::new(
                &text,
                &shortcut_player,
                &message_type,
                text.trim(),
                command_observation.clone(),
            ) && envelope.prefix() == CommandPrefix::Hash
                && let Some(parsed) =
                    router.route(&envelope, self.business.business.active_entertainment()?)
            {
                outcome.processed = true;
                outcome.command_dispatched |= self.submit_secondary_command(parsed)?;
                continue;
            }
            if accepts_turtle_questions {
                let question = message_sender
                    .as_deref()
                    .map(str::trim)
                    .filter(|player| !player.is_empty())
                    .and_then(|player| turtle_soup::parse_question_message(&text, Some(player)));
                if let Some(question) = question {
                    self.enqueue_turtle_soup_question(question, frame.captured_at())?;
                    outcome.processed = true;
                    continue;
                }
            }
            let Some(index) = text.find(['@', '＠']) else {
                log::debug!("二级监听气泡不是命令: {}", redacted_chat_text(&text));
                outcome.unrecognized = true;
                continue;
            };
            let command_text = text[index..].trim().to_string();
            let Some(envelope) = CommandEnvelope::new(
                &text,
                &shortcut_player,
                &message_type,
                command_text,
                command_observation,
            ) else {
                outcome.unrecognized = true;
                continue;
            };
            let Some(parsed) =
                router.route(&envelope, self.business.business.active_entertainment()?)
            else {
                log::debug!("二级监听气泡未解析为命令");
                outcome.unrecognized = true;
                continue;
            };
            outcome.processed = true;
            outcome.command_dispatched |= self.submit_secondary_command(parsed)?;
        }
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use crate::features::entertainment::EntertainmentKind;

    use super::*;

    #[test]
    fn secondary_permission_check_reads_sender_before_rejecting_mapped_administrator() {
        use crate::features::identity::{IdentityAccess, IdentityConfig, IdentityMapping};

        let identity = IdentityAccess::new(IdentityConfig {
            mappings: vec![IdentityMapping {
                nickname: "管理员".to_string(),
                id: uuid::Uuid::from_u128(1),
                role: IdentityRole::Admin,
                note: String::new(),
            }],
        });
        let service = CustomWorkflowService::new(
            crate::features::custom_workflow::CustomWorkflowConfig::default(),
            WorkflowDefaults {
                default_timeout_ms: 1_000,
                default_poll_ms: 100,
                default_step_wait_ms: 100,
                decision_timeout_ms: 1_000,
                decision_poll_ms: 100,
                after_activate_ms: 100,
                clipboard_hold_ms: 100,
                stability_mean_threshold: 1.0,
                stability_changed_ratio_threshold: 0.1,
            },
        );
        let router = ChatCommandRouter::with_identity(&service, &identity);
        for text in ["@禁用", "@闲置退出 15", "@删除", "@监听模式 一级"] {
            let envelope = |sender: &str| {
                CommandEnvelope::new(text, sender, "blue", text, CommandObservation::default())
                    .unwrap()
            };
            let provisional = router
                .route(&envelope(SECONDARY_HALL_FALLBACK_SENDER), None)
                .unwrap();
            assert_eq!(provisional.permission_required, Some(IdentityRole::Admin));
            let classification =
                classify_secondary_hall_routed_message(text, Some(&provisional), false, true);
            assert_eq!(classification.kind, SecondaryHallMessageKind::Command);
            assert!(classification.requires_sender, "command={text}");

            let authorized = router.route(&envelope("管理员"), None).unwrap();
            assert_eq!(authorized.permission_required, None, "command={text}");
            assert_eq!(authorized.role, Some(IdentityRole::Admin));
        }
        let disabled = classify_secondary_hall_routed_message("@启用", None, false, false);
        assert_eq!(disabled.kind, SecondaryHallMessageKind::Command);
        assert!(disabled.requires_sender);
        for text in ["#状态", "#结束"] {
            let envelope = |sender: &str| {
                CommandEnvelope::new(text, sender, "blue", text, CommandObservation::default())
                    .unwrap()
            };
            let provisional = router
                .route(
                    &envelope(SECONDARY_HALL_FALLBACK_SENDER),
                    Some(EntertainmentKind::TurtleSoup),
                )
                .unwrap();
            let classification =
                classify_secondary_hall_routed_message(text, Some(&provisional), false, false);
            assert_eq!(classification.kind, SecondaryHallMessageKind::Command);
            assert!(classification.requires_sender, "disabled command={text}");
            let authorized = router
                .route(&envelope("管理员"), Some(EntertainmentKind::TurtleSoup))
                .unwrap();
            assert_eq!(authorized.permission_required, None);
            assert_eq!(authorized.role, Some(IdentityRole::Admin));
        }
    }

    fn command(text: &str) -> SecondaryOcrMessage {
        SecondaryOcrMessage {
            text: text.to_string(),
            sender: None,
            sender_identity: None,
            kind: SecondaryHallMessageKind::Command,
            requires_sender: false,
        }
    }

    fn actor_command(text: &str, nickname: &str, remark: Option<&str>) -> SecondaryOcrMessage {
        let identity = SecondarySenderIdentity {
            nickname: nickname.to_string(),
            remark: remark.map(str::to_string),
        };
        SecondaryOcrMessage {
            text: text.to_string(),
            sender: Some(identity.effective_name().to_string()),
            sender_identity: Some(identity),
            kind: SecondaryHallMessageKind::Command,
            requires_sender: true,
        }
    }

    #[test]
    fn unread_completion_guard_releases_claim_after_error_and_panic() {
        let runtime = BusinessRuntime::start(
            8,
            IdiomChainService::from_entries_for_test(&["画蛇添足", "足智多谋"], None),
            CardGameService::new(LandlordConfig::default()),
        )
        .unwrap();
        let business = runtime.handle();
        assert!(
            business
                .request_chat_listener_mode(ChatListenerMode::Secondary)
                .unwrap()
                .0
        );
        business
            .complete_chat_listener_mode(ChatListenerMode::Secondary)
            .unwrap();
        business
            .finish_chat_listener_initial_unread_clear()
            .unwrap();
        business.finish_chat_listener_hall_round().unwrap();

        assert!(business.claim_chat_listener_unread_task().unwrap());
        let failed: Result<()> = {
            let _completion = SecondaryUnreadTaskCompletionGuard::new(business.clone());
            Err(anyhow!("模拟未读任务提前失败"))
        };
        assert!(failed.is_err());
        assert!(
            !business
                .chat_listener_snapshot()
                .unwrap()
                .unread_task_pending
        );

        assert!(business.claim_chat_listener_unread_task().unwrap());
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _completion = SecondaryUnreadTaskCompletionGuard::new(business.clone());
            panic!("模拟未读任务 panic");
        }));
        assert!(unwind.is_err());
        assert!(
            !business
                .chat_listener_snapshot()
                .unwrap()
                .unread_task_pending
        );
        assert!(business.claim_chat_listener_unread_task().unwrap());
        business.release_chat_listener_unread_task().unwrap();

        assert!(business.claim_chat_listener_unread_task().unwrap());
        {
            let mut completion = SecondaryUnreadTaskCompletionGuard::new(business.clone());
            completion.mark_message_processed();
        }
        let snapshot = business.chat_listener_snapshot().unwrap();
        assert!(!snapshot.unread_task_pending);
        assert!(snapshot.hall_round_required);

        runtime.shutdown().unwrap();
    }

    #[test]
    fn command_tracker_matches_scroll_and_keeps_new_bottom_command() {
        let mut tracker = SecondaryHallCommandTracker::default();
        let previous = vec![command("@帮助"), command("@点歌 测试")];
        tracker.commit(&previous);

        let current = vec![command("@点歌 测试"), command("@状态")];
        assert_eq!(tracker.preview_new_indexes(&current), vec![1]);
        tracker.commit(&current);

        let scrolled = vec![command("@状态")];
        assert!(tracker.preview_new_indexes(&scrolled).is_empty());
    }

    #[test]
    fn command_tracker_uses_remark_for_actor_dependent_command_identity() {
        let mut tracker = SecondaryHallCommandTracker::default();
        let previous = vec![actor_command("#出 3", "昵称", Some("备注"))];
        tracker.commit(&previous);

        let same_actor = vec![actor_command("＃出 3", "昵称", Some("备注"))];
        assert!(tracker.preview_new_indexes(&same_actor).is_empty());

        tracker.commit(&[actor_command("#出 3", "昵称", None)]);
        let remark_loaded = vec![actor_command("#出 3", "昵称", Some("备注"))];
        assert!(tracker.preview_new_indexes(&remark_loaded).is_empty());
        tracker.commit(&remark_loaded);

        let different_actor = vec![actor_command("#出 3", "昵称", Some("另一备注"))];
        assert_eq!(tracker.preview_new_indexes(&different_actor), vec![0]);
    }

    #[test]
    fn command_tracker_keeps_a_sender_pending_candidate_visible_for_retry() {
        let mut tracker = SecondaryHallCommandTracker::default();
        tracker.commit(&[]);
        let pending = vec![SecondaryOcrMessage {
            text: "#出 3".to_string(),
            sender: None,
            sender_identity: None,
            kind: SecondaryHallMessageKind::Command,
            requires_sender: true,
        }];
        assert_eq!(tracker.preview_new_indexes(&pending), vec![0]);
    }

    #[test]
    fn command_tracker_detects_structured_song_ocr_as_a_new_command() {
        let mut tracker = SecondaryHallCommandTracker::default();
        let previous = vec![command("@帮助")];
        tracker.commit(&previous);

        let current = vec![command("歌曲名：醉，歌手名：三无Marblue，专辑名：")];
        assert_eq!(tracker.preview_new_indexes(&current), vec![0]);

        tracker.commit(&current);
        assert!(tracker.preview_new_indexes(&current).is_empty());
    }
}
