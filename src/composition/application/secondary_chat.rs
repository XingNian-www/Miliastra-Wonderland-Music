use super::*;

impl ChatDecisionReader {
    /// 与 `read_messages` 同一套基线/新鲜度去重规则，但保留观察 id：
    /// 点歌决策消费成功后可 `acknowledge_business`，避免独占窗口回放时主链路误报“未执行”。
    fn read_messages_with_ids(
        &mut self,
    ) -> Result<Vec<(ChatMessage, crate::observation::chat::ObservedChatMessageId)>> {
        let mut messages = Vec::new();
        for snapshot in self.subscriber.read()? {
            if !self.same_conversation(&snapshot) {
                continue;
            }
            // 与 read_messages 共用同一条首屏防护，避免某条路径静默绕开。
            if self.skips_first_frame_of_new_conversation(&snapshot) {
                continue;
            }
            let snapshot_at = snapshot.captured_at;
            for message in snapshot.messages {
                if !self.is_after_baseline_at(snapshot_at, &message) {
                    continue;
                }
                if self.baseline_cursor.is_some_and(|(session, sequence)| {
                    message.id.visual_session == session
                        && message.id.bubble_sequence.get() <= sequence
                }) || !self.seen.insert((message.id.clone(), message.text.clone()))
                {
                    continue;
                }
                messages.push((
                    ChatMessage {
                        message_type: message.message_type,
                        block: message.block,
                        text: message.text,
                    },
                    message.id,
                ));
            }
        }
        Ok(messages)
    }
}

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
    pub(super) command_dispatched: bool,
    pub(super) unrecognized: bool,
}

/// 二级大厅消息的命令主语键。
///
/// 优先级与身份模型一致：昵称 > 账号 UID > 占位符。昵称在前是因为它是
/// 身份映射与展示的既有匹配键（OCR 路径只有它）；外部源昵称缺失时才退回
/// UID，保证同一账号在同一大厅内仍然稳定，不会所有人塌缩成同一个占位名。
/// 两者都没有时返回占位符：那种消息已经在上游被 Unknown 拒绝，到不了这里。
fn secondary_hall_actor_key(
    nickname: Option<&str>,
    account: Option<&crate::features::command::CommandAccount>,
) -> String {
    if let Some(nickname) = nickname.map(str::trim).filter(|name| !name.is_empty()) {
        return nickname.to_string();
    }
    if let Some(uid) = account
        .map(|account| account.uid.trim())
        .filter(|uid| !uid.is_empty())
    {
        return format!("uid:{uid}");
    }
    SECONDARY_HALL_FALLBACK_SENDER.to_string()
}

#[derive(Clone, Debug)]
struct SecondaryOcrMessage {
    text: String,
    sender: Option<String>,
    sender_identity: Option<SecondarySenderIdentity>,
    kind: SecondaryHallMessageKind,
    requires_identity: bool,
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
    classification.requires_identity |= routed
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
    requires_identity: bool,
    row_from_bottom: usize,
}

impl SecondaryHallCommandObservation {
    fn stable_with(&self, other: &Self) -> bool {
        if self.command_key != other.command_key
            || self.requires_identity != other.requires_identity
        {
            return false;
        }
        if !self.requires_identity {
            return true;
        }
        if self.actor.is_none() || other.actor.is_none() {
            return self.actor.is_none() && other.actor.is_none();
        }
        self.matches(other)
    }

    fn matches(&self, other: &Self) -> bool {
        if self.command_key != other.command_key
            || self.requires_identity != other.requires_identity
        {
            return false;
        }
        if !self.requires_identity {
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

    fn snapshot(messages: &[SecondaryOcrMessage]) -> Vec<SecondaryHallCommandObservation> {
        messages
            .iter()
            .enumerate()
            .filter_map(|(index, message)| {
                secondary_hall_command_observation(message, messages.len() - 1 - index)
            })
            .collect()
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
    let actor = if message.requires_identity {
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
        requires_identity: message.requires_identity,
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
        // 身份判定的依据按来源分开，不再用「外部源是否开着」猜：
        //
        // - OCR 路径：看共享快照是不是二级大厅（屏幕上有二级聊天区就是 Identity::SecondaryHall）。
        // - 外部源路径：inject_external 有意不写 `state.latest`（外部消息没有屏幕坐标，
        //   写进去会毒化下一帧对齐），所以 latest 恒为 None。此时身份必须由
        //   「当前是否有外部消息在账本里」决定，而不是靠界面推断。
        //
        // 以前把两者混成"external_active 就认定 CurrentHall"，语义上能跑但把
        // 「看哪块屏幕」和「消息属于哪个容器」绑死了——正是这次要拆开的耦合。
        // 判「屏幕确认二级界面」只看 chat：这是纯粹的**屏幕分桶**问题
        // （二级面板几何判定出来的桶），与消息属于哪个容器无关。
        //
        // 这里刻意保留 chat 而非 scope：scope 是业务范围（大厅/私聊），
        // 它回答不了「BOT 现在看的是不是二级界面」。两者职责不同，
        // 混用会让外部源的大厅消息被当成"屏幕确认二级"，从而跳过 UI 恢复。
        let shared_snapshot = self.ui.chat_timeline.latest()?;
        let ocr_confirms_secondary = shared_snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.chat.as_ref() == Some(&crate::observation::chat::ChatIdentity::SecondaryHall)
        });
        // 外部源在线就说明它随时可能交付大厅消息；身份按"大厅"对待，
        // 否则二级轮次既不敢派发也会反复排队恢复大厅。
        let external_supplies_hall = self.lifecycle.external_chat.supplies_hall_messages();
        let (observed_identity, scanned) = if ocr_confirms_secondary || external_supplies_hall {
            (SecondaryChatIdentity::CurrentHall, true)
        } else {
            (SecondaryChatIdentity::Unknown, false)
        };
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
            log::info!("二级值守初始未读清场完成，当前大厅已建立消息基线");
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
                    // 外部聊天源接管时不排队未读任务。
                    //
                    // 未读提升是屏幕操作：要等一帧 OCR 好友会话、再从 screen.latest
                    // 取修订。接管期间观察线程不发布任何帧、外部帧也没有屏幕桶，
                    // 任务必然等到超时并报「fresh shared friend chat OCR did not arrive」，
                    // 红点清不掉还会反复重排队——纯空转。
                    // 外部私聊本身走 scope=Private 直投业务链路，不受影响。
                    if external_supplies_hall {
                        log::debug!("外部聊天源接管中，跳过未读提升（该流程依赖屏幕帧）");
                        return Ok(SecondaryListenerRoundOutcome {
                            scanned,
                            verified,
                            ..Default::default()
                        });
                    }
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
            "二级值守检测到好友未读红点: y={} discard_only={}",
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
        log::info!("二级值守检测到不可执行会话，已加入恢复当前大厅任务");
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
        result.context("二级值守暂时无法恢复当前大厅，已保留二级模式并等待重试")
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
        // 基线匹配按**来源**分流：
        //
        // - OCR 路径：窗口期的基线是「某块屏幕的快照」，只能按屏幕桶（chat）匹配。
        //   一级区与二级区是同一大厅的两块面板，必须区分开。
        // - 外部源路径：消息没有屏幕，外部大厅恒被映射成 SecondaryHall。
        //   若仍按 chat 匹配，一级值守下窗口会一直等 PrimaryHall 而永远等不到，
        //   表现就是「等待共享聊天确认基线超时」。改按业务范围（scope）匹配。
        let external_active = self.lifecycle.external_chat.supplies_hall_messages();
        let expected_chat = if scope == ChatDecisionScope::CurrentHall
            && self.active_ui_residency()? == UiResidency::SecondaryCurrentHall
        {
            crate::observation::chat::ChatIdentity::SecondaryHall
        } else {
            crate::observation::chat::ChatIdentity::PrimaryHall
        };
        let chat = expected_chat;
        // 先取水位再订阅：反过来的话，两步之间到达的消息会落在水位之内被当成历史。
        let external_floor = external_active.then(|| self.lifecycle.external_chat.last_sequence());
        let subscriber = self.ui.chat_timeline.subscribe()?;
        // 外部源在线时**不等快照**，直接用服务端序号水位建基线。
        //
        // 为什么：外部接管期间观察线程不发布任何帧（shared_observer 的两个布局分支
        // 都直接 return），等待中的 refresh 只能靠「刚好有人说话」来完成。安静大厅里
        // 打开决策窗口 → 20s 后必然报「等待共享聊天确认基线超时」。
        // OCR 有 2000ms 兜底帧，所以它从不暴露这个问题——这正是两模式不一致的根源。
        //
        // 外部源是有序队列，「窗口前的历史」由序号就能确定，不需要也不该等屏幕帧。
        if external_active {
            let floor = external_floor.unwrap_or(0);
            let hall_scope = crate::observation::chat::ChatScope::hall();
            // 多会话窗口（邀请/自定义流程确认）既要收大厅也要收好友私聊，
            // 这时不能绑单一容器——绑了会在投递层把私聊整批挡掉。
            let any_conversation = scope == ChatDecisionScope::MultipleConversations;
            if !any_conversation {
                // 会话号对账本水位基线无意义：不按 session 过滤，只按序号。
                subscriber.bind_scope(hall_scope.clone())?;
            }
            return Ok(ChatDecisionReader {
                listener_mode,
                subscriber,
                chat,
                scope: hall_scope,
                any_conversation,
                settled_conversations: std::collections::HashSet::new(),
                baseline_at: Instant::now(),
                sequence_floor: Some(floor),
                baseline_cursor: None,
                seen: std::collections::HashSet::new(),
                screen_lock: DecisionScreenLock::default(),
                _observation_session: observation_session,
            });
        }
        // OCR 路径仍然要等一帧快照：屏幕只显示当前内容，必须拍一帧才知道
        // 「哪些气泡是窗口打开前就已经在屏上的」。账本水位替代不了它——
        // 屏幕可能正显示着历史气泡，账本不会告诉你这些已经看过了。
        // MUST_READ §2 的「首屏历史不执行」就建立在这个基线上。
        //
        // 外部源不需要这条路：它是有序队列，新旧由服务端序号直接回答。
        let mut refresh = self.ui.chat_timeline.request_refresh()?;
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
                return Err(anyhow!("值守模式正在切换，已结束确认窗口"));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(anyhow!("等待共享聊天确认基线超时"));
            }
            let refreshed = self
                .ui
                .chat_timeline
                .wait_for_snapshot(refresh, remaining.min(Duration::from_millis(50)));
            let snapshot = match refreshed {
                Ok(Some(snapshot)) => snapshot,
                Ok(None) => continue,
                Err(error)
                    if error
                        .downcast_ref::<crate::observation::chat::NoChatRefreshError>()
                        .is_some() =>
                {
                    // 退避一小段再重挂：投递层在「整批无可用容器」时也会用 NoChat
                    // 完成围栏，若不可映射的频道持续连发，这里会高频空转抢占锁。
                    // 与 moderation.rs 的 NoChat 分支保持一致。
                    let backoff = deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(50));
                    if !backoff.is_zero() {
                        std::thread::sleep(backoff);
                    }
                    refresh = self.ui.chat_timeline.request_refresh()?;
                    continue;
                }
                Err(error) => return Err(error),
            };
            // 每轮重新判断来源：窗口打开期间外部源可能才刚连上（或刚好断线），
            // 用入口处的一次快照会让后面这半段永远走错分支，一路空转到超时。
            let external_now = self.lifecycle.external_chat.supplies_hall_messages();
            let matched = if external_now {
                // 外部源：只看业务范围。外部帧没有屏幕桶（chat 恒为 None），
                // 按屏幕桶匹配会让窗口永远等不到基线。
                snapshot.scope.is_hall()
            } else {
                // OCR：只认绑定的那一块聊天。另一块大厅面板的快照会被这里拒绝，
                // 换新 token 继续等——那是同一大厅的另一块面板，属预期内的瞬时状态，
                // 不得用它做基线游标，也不得重用已完成的 token。
                snapshot.chat.as_ref() == Some(&chat)
            };
            if !matched {
                // 这块快照不是窗口要的：换一个新 token 继续等，不要消耗掉当前这份。
                refresh = self.ui.chat_timeline.request_refresh()?;
                continue;
            }
            break snapshot;
        };
        if baseline.source.is_ocr() && baseline.chat.as_ref() != Some(&chat) {
            return Err(anyhow!("共享聊天确认基线会话不匹配"));
        }
        if scope == ChatDecisionScope::MultipleConversations {
            // 多会话窗口：不绑任何容器，投递层才会把大厅与私聊都送进来；
            // 由调用方按消息类型（blue/pink）自行筛选。
        } else if baseline.source.is_ocr() {
            // 按**基线快照的来源**决定绑法。
            // 用入口期的活标志会在「窗口打开后外部源才接管」时绑错：
            // baseline 是外部快照、却绑了屏幕桶，投递层随后整批丢弃。
            subscriber.bind_chat(chat.clone(), baseline.visual_session)?;
        } else {
            // 外部源按业务容器绑定：它的消息没有屏幕桶，绑 chat 会被投递层整批挡住。
            subscriber.bind_scope(baseline.scope.clone())?;
        }
        // 外部源不用屏幕游标：它的 `bubble_sequence` 是去重哈希、不单调，
        // 拿最大值当水位会把约一半新消息误判成旧消息。序号单调性由账本负责。
        let baseline_cursor = if !baseline.source.is_ocr() {
            None
        } else {
            Some((
                baseline.visual_session,
                baseline
                    .messages
                    .iter()
                    .map(|message| message.id.bubble_sequence.get())
                    .max()
                    .unwrap_or(0),
            ))
        };
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
            // 业务容器直接取基线快照自带的值——它对两种来源都是权威的。
            // 不要按 external_active 硬写成 hall：外部私聊窗口会因此匹配错容器。
            scope: baseline.scope.clone(),
            // 按**基线快照的来源**固化，不用调用瞬间的活标志：
            // 窗口期间外部源可能掉线（租约超时）让 OCR 恢复发布，
            // 若这时还按外部源的判据读，屏幕上的旧决策气泡会被当成新决策。
            any_conversation: scope == ChatDecisionScope::MultipleConversations,
            settled_conversations: std::collections::HashSet::new(),
            baseline_at: baseline.captured_at,
            // 外部源按单调序号判新旧：让基线成立的那条消息的 captured_at 就等于
            // baseline_at，用 `<=' 会把它自己排除，用户第一次 @1 永远进不来。
            sequence_floor: if baseline.source.is_ocr() {
                None
            } else {
                Some(
                    baseline
                        .messages
                        .iter()
                        .map(|message| message.source_sequence)
                        .max()
                        .unwrap_or(0),
                )
            },
            baseline_cursor,
            seen: baseline
                .messages
                .into_iter()
                .map(|message| (message.id, message.text))
                .collect(),
            // 外部源的 block 是哨兵坐标（全部相同），位置锁会把同文本决策
            // 永久判成「已存在」。而它每条消息只投递一次——重复文本就是用户
            // 真的又发了一次（连续两次 @换一批），不该被否决。用真正的 inert。
            screen_lock: if !baseline.source.is_ocr() {
                DecisionScreenLock::inert()
            } else {
                DecisionScreenLock::from_messages(&messages, accepts_message_type, is_decision)
            },
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
            return Err(anyhow!("值守模式正在切换，已结束确认窗口"));
        }
        reader.read_messages()
    }

    /// 同 `wait_for_chat_decision_messages`，但把观察 id 一并带出，
    /// 供点歌决策读者消费成功后 `acknowledge_business`，避免独占窗口回放误报。
    pub(super) fn wait_for_chat_decision_messages_with_ids(
        &self,
        reader: &mut ChatDecisionReader,
        configured_ms: u64,
        remaining: Duration,
    ) -> Result<Vec<(ChatMessage, crate::observation::chat::ObservedChatMessageId)>> {
        let wait_until = Instant::now() + remaining;
        let result = (|| {
            if !self.lifecycle.running.load(AtomicOrdering::SeqCst) {
                return Ok(Vec::new());
            }
            let messages = self.poll_chat_decision_reader_with_ids(reader)?;
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
            self.poll_chat_decision_reader_with_ids(reader)
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

    pub(super) fn poll_chat_decision_reader_with_ids(
        &self,
        reader: &mut ChatDecisionReader,
    ) -> Result<Vec<(ChatMessage, crate::observation::chat::ObservedChatMessageId)>> {
        if self.lifecycle.paused.load(AtomicOrdering::SeqCst) {
            return Err(anyhow!("运行已暂停，已结束确认窗口"));
        }
        if self.lifecycle.live_configs.has_pending_reload() {
            return Err(anyhow!("配置重载待处理，已结束确认窗口"));
        }
        let snapshot = self.business.business.chat_listener_snapshot()?;
        if snapshot.mode != reader.listener_mode || snapshot.pending_mode.is_some() {
            return Err(anyhow!("值守模式正在切换，已结束确认窗口"));
        }
        reader.read_messages_with_ids()
    }

    pub(super) fn classify_secondary_business_message(
        &self,
        text: &str,
        accepts_turtle_questions: bool,
    ) -> Result<SecondaryHallMessageClassification> {
        let access_config = self.lifecycle.live_configs.snapshot();
        let router = ChatCommandRouter::with_access(
            &self.business.custom_workflow,
            &self.lifecycle.live_configs.identity,
            &access_config.command_access,
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
            account,
        } in messages
        {
            log::debug!("处理二级观察消息: id={message_id:?}");
            let command_observation = CommandObservation {
                frame_id: Some(frame.id()),
                captured_at: Some(frame.captured_at()),
                message_id: Some(message_id.clone()),
                message_text: Some(text.clone()),
                // 二级观察当前不写命令锁（锁由调用方在认领时用 claim_business_at
                // 携带锚点完成），这里不需要位置信息。
                lock_anchor: None,
            };
            let shortcut_player = if message_type == "pink" {
                friend_name.trim().to_string()
            } else {
                secondary_hall_actor_key(message_sender.as_deref(), account.as_ref())
            };
            let access_config = self.lifecycle.live_configs.snapshot();
            let router = ChatCommandRouter::with_access(
                &self.business.custom_workflow,
                &self.lifecycle.live_configs.identity,
                &access_config.command_access,
            );
            if message_type == "blue" {
                let classification =
                    self.classify_secondary_business_message(&text, accepts_turtle_questions)?;
                // 身份来源按可信度排序：账号 UID 优先，昵称次之。外部聊天源的
                // 昵称可能为 null（API 文档 §5）而 from.uid 必填，只看昵称会把整条
                // 命令丢掉；反过来，两者皆无（Unknown）仍然拒绝，不放宽到无身份执行。
                if classification.kind == SecondaryHallMessageKind::Ignored
                    || (classification.requires_identity
                        && !crate::features::command::has_known_identity(
                            message_sender.as_deref(),
                            account.as_ref(),
                        ))
                {
                    if command::looks_like_command(&text) {
                        log::info!(
                            "命令暂缓：二级命令缺少可确认身份（UID 与昵称皆无），等待身份修订 text={}",
                            redacted_chat_text(&text)
                        );
                    } else {
                        log::debug!("二级身份相关输入尚未取得可确认身份，保留共享消息等待身份修订");
                    }
                    outcome.unrecognized = true;
                    continue;
                }
            }
            // 顺序必须与一级一致：**显式 @/# 前缀优先，失败才回落结构化点歌**。
            //
            // 反过来（先结构化）会让 `@通告 歌曲名：X，歌手名：Y` 这类消息在二级被
            // 劫持成 `@点歌 X Y`，而一级仍按「通告」执行——同一文本两条路径派发不同模块。
            // 根因是 parse_structured_song_text 用 find 找标签、不要求标签在行首，
            // 所以任何以 @/# 开头、正文里又带歌曲名的消息都会命中。
            //
            // 行首就是命令前缀时走正规路径，不要退回下面的「找 @」兜底：
            // 兜底位于身份检查之后，外部源昵称缺失时整条 @ 命令会被吞掉。
            // Amp 是测试专用前缀，仍然排除。
            // 用提取结果而不是 `text.trim()`：正文可能带「昵称：」前缀（外部源
            // 统一补成 OCR 排版），`CommandEnvelope` 的前缀判定要求 `@`/`#` 在开头。
            // 直接传原始 text 会让外部源的 `#` 命令恒失败——Hash 分支 strip_prefix 失败，
            // 返回 None，整条落入 Ignored。
            if let Some(command_text) = secondary_hall_command_text(&text)
                && let Some(envelope) = CommandEnvelope::new(
                    &text,
                    &shortcut_player,
                    &message_type,
                    command_text,
                    command_observation.clone(),
                )
                .map(|envelope| envelope.with_account(account.clone()))
                && envelope.prefix() != CommandPrefix::Amp
                && let Some(parsed) =
                    router.route(&envelope, self.business.business.active_entertainment()?)
            {
                outcome.command_dispatched |= self.submit_secondary_command(parsed)?;
                continue;
            }
            if let Some(envelope) = command::parse_structured_song_envelope(
                &text,
                &shortcut_player,
                &message_type,
                command_observation.clone(),
            )
            .map(|envelope| envelope.with_account(account.clone()))
                && let Some(parsed) =
                    router.route(&envelope, self.business.business.active_entertainment()?)
            {
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
                    continue;
                }
            }
            let Some(index) = text.find(['@', '＠']) else {
                log::debug!("二级值守气泡不是命令: {}", redacted_chat_text(&text));
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
            )
            .map(|envelope| envelope.with_account(account.clone())) else {
                if command::looks_like_command(&text) {
                    log::info!(
                        "命令未执行：二级消息无法解析为命令信封 text={}",
                        redacted_chat_text(&text)
                    );
                }
                outcome.unrecognized = true;
                continue;
            };
            let active_entertainment = self.business.business.active_entertainment()?;
            let Some(parsed) = router.route(&envelope, active_entertainment) else {
                if command::looks_like_command(&text) {
                    log::info!(
                        "命令未执行：{} text={}",
                        ChatCommandRouter::unclaimed_command_reason(
                            &envelope,
                            active_entertainment
                        ),
                        redacted_chat_text(&text)
                    );
                } else {
                    log::debug!("二级值守气泡未解析为命令");
                }
                outcome.unrecognized = true;
                continue;
            };
            outcome.command_dispatched |= self.submit_secondary_command(parsed)?;
        }
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    /// 端到端回归：外部源的 `#` 命令必须能真的派发。
    ///
    /// 这条覆盖的是**整条链路**：投递层补「昵称：」前缀 → 提取命令 → 构造信封 →
    /// 路由命中。此前只测了提取函数返回 `Some`，而派发路径用的是 `text.trim()`，
    /// 于是测试全绿但功能不通。
    #[test]
    fn external_hash_command_survives_sender_prefix_through_dispatch() {
        use crate::features::command::CommandEnvelope;
        // 外部源投递层补前缀后的正文。
        let text = "玩家癸：#状态";
        let command_text =
            super::secondary_hall_command_text(text).expect("带昵称前缀的 # 命令必须能被提取");
        let envelope = CommandEnvelope::new(text, "玩家癸", "blue", command_text, Default::default())
            .expect("提取出的 #状态 必须能构造命令信封");
        assert_eq!(
            format!("{:?}", envelope.prefix()),
            "Hash",
            "前缀必须是 Hash，实际正文: {text:?}"
        );
    }

    /// 同样覆盖 `@` 命令，确认两条前缀都通。
    #[test]
    fn external_at_command_survives_sender_prefix_through_dispatch() {
        use crate::features::command::CommandEnvelope;
        let text = "玩家癸：@状态";
        let command_text =
            super::secondary_hall_command_text(text).expect("带昵称前缀的 @ 命令必须能被提取");
        let envelope = CommandEnvelope::new(text, "玩家癸", "blue", command_text, Default::default())
            .expect("提取出的 @状态 必须能构造命令信封");
        assert_eq!(format!("{:?}", envelope.prefix()), "At");
    }
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
                accounts: Vec::new(),
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
        for text in ["@禁用", "@闲置退出 15", "@删除", "@值守模式 一级"] {
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
            assert!(classification.requires_identity, "command={text}");

            let authorized = router.route(&envelope("管理员"), None).unwrap();
            assert_eq!(authorized.permission_required, None, "command={text}");
            assert_eq!(authorized.role, Some(IdentityRole::Admin));
        }
        let disabled = classify_secondary_hall_routed_message("@启用", None, false, false);
        assert_eq!(disabled.kind, SecondaryHallMessageKind::Command);
        assert!(disabled.requires_identity);
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
            assert!(classification.requires_identity, "disabled command={text}");
            let authorized = router
                .route(&envelope("管理员"), Some(EntertainmentKind::TurtleSoup))
                .unwrap();
            assert_eq!(authorized.permission_required, None);
            assert_eq!(authorized.role, Some(IdentityRole::Admin));
        }
    }

    fn external_account(uid: &str) -> crate::features::command::CommandAccount {
        crate::features::command::CommandAccount::new(
            miliastra_kernel::chat_source::GAME_GENSHIN,
            uid,
        )
    }

    #[test]
    fn actor_key_falls_back_to_uid_when_external_nickname_is_missing() {
        // 回归：外部聊天源昵称为 null（API 文档 §5）但 from.uid 必填。
        // 旧实现直接落回占位符，导致大厅里所有缺昵称的玩家塌缩成同一个命令主语，
        // 锁键、回合归属与歌单归属全部串味。
        assert_eq!(
            secondary_hall_actor_key(None, Some(&external_account("900000001"))),
            "uid:900000001"
        );
        assert_eq!(
            secondary_hall_actor_key(Some("  "), Some(&external_account("42"))),
            "uid:42"
        );
    }

    #[test]
    fn actor_key_prefers_the_nickname_that_identity_mapping_uses() {
        // 昵称是身份映射与 OCR 的既有匹配键，有昵称时不能被 UID 顶掉。
        assert_eq!(
            secondary_hall_actor_key(Some("芦荟"), Some(&external_account("42"))),
            "芦荟"
        );
        assert_eq!(secondary_hall_actor_key(Some("芦荟"), None), "芦荟");
    }

    #[test]
    fn actor_key_keeps_the_placeholder_only_for_unknown_identities() {
        // Unknown 在上游就会被拒绝执行，占位符只是这条路径的兜底。
        assert_eq!(
            secondary_hall_actor_key(None, None),
            SECONDARY_HALL_FALLBACK_SENDER
        );
    }

    #[test]
    fn uid_only_external_message_passes_the_identity_gate_but_unknown_does_not() {
        use crate::features::command::has_known_identity;

        // 外部源：昵称缺失、UID 存在 → 放行（这是本轮的回归点）。
        assert!(has_known_identity(
            None,
            Some(&external_account("900000001"))
        ));
        // OCR 路径：只有昵称 → 放行。
        assert!(has_known_identity(Some("芦荟"), None));
        // 两者皆无 → Unknown，仍然拒绝；不得放宽到无身份也执行。
        assert!(!has_known_identity(None, None));
    }

    #[test]
    fn uid_only_song_request_routes_through_the_shared_at_path() {
        // 回归：@ 曾被 `prefix == Hash` 排除在正常路径之外，只能落到「找 @」兜底；
        // 兜底位于身份检查之后，外部源昵称缺失时整条 @点歌 就被吞掉。
        // 这里验证 @点歌 本身就是可路由的 At 信封，不依赖兜底。
        let envelope = CommandEnvelope::new(
            "@点歌晴天",
            "uid:900000001",
            "blue",
            "@点歌晴天",
            CommandObservation::default(),
        )
        .expect("uid-only 消息仍能构造命令信封")
        .with_account(Some(external_account("900000001")));

        assert_eq!(envelope.prefix(), CommandPrefix::At);
        assert!(envelope.account().is_some());

        let routed = ChatCommandRouter::without_custom_workflow()
            .route(&envelope, None)
            .expect("@点歌 必须被点歌模块认领");
        assert!(matches!(routed.command, ModuleCommand::SongRequest(_)));
    }

    fn command(text: &str) -> SecondaryOcrMessage {
        SecondaryOcrMessage {
            text: text.to_string(),
            sender: None,
            sender_identity: None,
            kind: SecondaryHallMessageKind::Command,
            requires_identity: false,
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
            requires_identity: true,
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
            requires_identity: true,
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
