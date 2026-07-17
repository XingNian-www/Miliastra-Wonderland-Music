use super::*;

struct SecondaryOcrMessage {
    text: String,
    sender: Option<String>,
    kind: SecondaryHallMessageKind,
    requires_sender: bool,
}

impl ApplicationRuntime {
    pub(super) fn run_secondary_listener_round(
        &mut self,
        image: &DynamicImage,
        last_friend_bubble: &mut Option<ChangeFingerprint>,
        hall_bubble_sequence: &mut Option<Vec<SecondaryHallBubble>>,
        last_title: &mut Option<ChangeFingerprint>,
        identity: &mut Option<SecondaryChatIdentity>,
    ) -> Result<bool> {
        if self.business.scheduler_snapshot()?.is_busy() {
            return Ok(false);
        }
        let title_fingerprint = rect_chat_change_fingerprint(image, SECONDARY_TITLE_RECT)?;
        let title_changed = identity.is_none()
            || last_title
                .as_ref()
                .is_none_or(|previous| secondary_fingerprint_changed(previous, &title_fingerprint));
        if title_changed {
            *identity = Some(self.secondary_identity_from_frame(image)?);
        }
        *last_title = Some(title_fingerprint);

        let state = self.business.chat_listener_snapshot()?;
        if state.unread_task_pending {
            return Ok(false);
        }
        let current_identity = identity.clone().unwrap_or(SecondaryChatIdentity::Unknown);

        if state.initial_unread_clear {
            if let Some(hit) = find_unread_friend_hits(image).into_iter().next() {
                return self.queue_secondary_unread_task(hit, true);
            }
            *last_friend_bubble = latest_incoming_fingerprint(image)?;
            *hall_bubble_sequence = Some(secondary_hall_bubbles(image)?);
            self.business.finish_chat_listener_initial_unread_clear()?;
            log::info!("二级监听初始未读清场完成，当前大厅已建立消息基线");
            return Ok(false);
        }

        match current_identity {
            SecondaryChatIdentity::CurrentHall => {
                if state.hall_round_required {
                    let scanned =
                        self.scan_secondary_hall_if_changed(image, hall_bubble_sequence)?;
                    self.business.finish_chat_listener_hall_round()?;
                    return Ok(scanned);
                }
                if let Some(hit) = find_unread_friend_hits(image).into_iter().next() {
                    return self.queue_secondary_unread_task(hit, false);
                }
                self.scan_secondary_hall_if_changed(image, hall_bubble_sequence)
            }
            SecondaryChatIdentity::Friend(name) => {
                if title_changed {
                    *last_friend_bubble = latest_incoming_fingerprint(image)?;
                    return Ok(false);
                }
                self.scan_secondary_latest_if_changed(image, "pink", &name, last_friend_bubble)
            }
            SecondaryChatIdentity::PublicChannel => self.queue_secondary_hall_recovery(),
            SecondaryChatIdentity::StrangerMessages => Ok(false),
            SecondaryChatIdentity::Unknown => Ok(false),
        }
    }

    fn queue_secondary_unread_task(
        &self,
        hit: UnreadFriendHit,
        discard_only: bool,
    ) -> Result<bool> {
        if !self.business.claim_chat_listener_unread_task()? {
            return Ok(false);
        }
        if let Err(error) =
            self.push_pending_task(PendingTask::SecondaryUnread { hit, discard_only })
        {
            self.business.release_chat_listener_unread_task()?;
            return Err(error);
        }
        log::info!(
            "二级监听检测到好友未读红点: y={} discard_only={}",
            hit.row_click.y,
            discard_only
        );
        Ok(false)
    }

    fn queue_secondary_hall_recovery(&self) -> Result<bool> {
        if !self.business.claim_chat_listener_unread_task()? {
            return Ok(false);
        }
        if let Err(error) = self.push_pending_task(PendingTask::RestoreSecondaryHall) {
            self.business.release_chat_listener_unread_task()?;
            return Err(error);
        }
        log::info!("二级监听检测到不可执行会话，已加入恢复当前大厅任务");
        Ok(false)
    }

    fn scan_secondary_latest_if_changed(
        &mut self,
        image: &DynamicImage,
        message_type: &str,
        friend_name: &str,
        last_bubble: &mut Option<ChangeFingerprint>,
    ) -> Result<bool> {
        let current = latest_incoming_fingerprint(image)?;
        let changed = match (&*last_bubble, &current) {
            (None, Some(_)) => true,
            (Some(previous), Some(current)) => secondary_fingerprint_changed(previous, current),
            _ => false,
        };
        if !changed {
            *last_bubble = current;
            return Ok(false);
        }

        let refreshed = self.wait_for_secondary_bubble_stability()?;
        let refreshed_fingerprint = latest_incoming_fingerprint(&refreshed.image)?;
        let outcome = self.process_secondary_latest_message(
            &refreshed.image,
            refreshed.captured_at,
            message_type,
            friend_name,
        )?;
        *last_bubble = refreshed_fingerprint;
        Ok(outcome)
    }

    fn scan_secondary_hall_if_changed(
        &mut self,
        image: &DynamicImage,
        previous: &mut Option<Vec<SecondaryHallBubble>>,
    ) -> Result<bool> {
        let current = secondary_hall_bubbles(image)?;
        match secondary_hall_sequence_delta(previous.as_deref(), &current) {
            SecondaryHallSequenceDelta::EstablishBaseline => {
                self.business.clear_turtle_soup_secondary_stability()?;
                *previous = Some(current);
                log::debug!("二级大厅气泡序列尚未建立，当前仅记录基线");
                return Ok(false);
            }
            SecondaryHallSequenceDelta::RetainedPrefix => {
                log::debug!("二级大厅当前只显示旧序列前缀，保留原基线等待完整观测");
                return Ok(false);
            }
            SecondaryHallSequenceDelta::NoChange => {
                self.business.clear_turtle_soup_secondary_stability()?;
                return Ok(false);
            }
            SecondaryHallSequenceDelta::LostOverlap | SecondaryHallSequenceDelta::NewFrom(_) => {}
        }

        let Some((refreshed, refreshed_bubbles)) = self.wait_for_secondary_hall_stability()? else {
            log::debug!("二级大厅新增消息尚未稳定，保留旧基线等待下一轮");
            return Ok(false);
        };
        let start = match secondary_hall_sequence_delta(previous.as_deref(), &refreshed_bubbles) {
            SecondaryHallSequenceDelta::NewFrom(start) => start,
            SecondaryHallSequenceDelta::RetainedPrefix => {
                log::debug!("二级大厅稳定观测仍只是旧序列前缀，保留原基线");
                return Ok(false);
            }
            SecondaryHallSequenceDelta::EstablishBaseline
            | SecondaryHallSequenceDelta::LostOverlap => {
                self.business.clear_turtle_soup_secondary_stability()?;
                *previous = Some(refreshed_bubbles);
                log::debug!("二级大厅气泡稳定后没有可靠重叠，已重建基线，不处理当前可见历史消息");
                return Ok(false);
            }
            SecondaryHallSequenceDelta::NoChange => {
                self.business.clear_turtle_soup_secondary_stability()?;
                *previous = Some(refreshed_bubbles);
                return Ok(false);
            }
        };
        let new_bubbles = &refreshed_bubbles[start..];

        log::info!(
            "二级大厅检测到 {} 条结构稳定的新增气泡，按显示顺序 OCR 正文",
            new_bubbles.len()
        );
        let outcome = self.process_secondary_bubble_rects(
            &refreshed.image,
            refreshed.captured_at,
            new_bubbles
                .iter()
                .map(|bubble| (bubble.rect, Some(bubble.sender_rect()))),
            "blue",
            "",
        )?;
        if outcome.confirmation_pending {
            log::debug!("二级大厅身份相关输入或海龟汤 OCR 尚未稳定，保留旧气泡基线等待下轮复核");
            return Ok(false);
        }
        *previous = Some(refreshed_bubbles);
        Ok(outcome.processed)
    }

    pub(super) fn execute_secondary_unread_task(
        &mut self,
        hit: UnreadFriendHit,
        discard_only: bool,
    ) -> Result<()> {
        let outcome = self
            .secondary_unread_ui
            .submit(ProcessSecondaryUnread::new(hit, discard_only))
            .context("提交二级好友未读 UI 事务")?
            .wait()
            .context("等待二级好友未读 UI 事务")?;
        let result = match outcome.effect() {
            SecondaryUnreadEffect::Message {
                captured_at,
                friend_name,
                text,
            } => {
                let frame = self.chat_observations.begin_frame(*captured_at)?;
                self.monitor.publish(MonitorEvent::Ocr(OcrSnapshot::new(
                    1,
                    vec![format!("[pink] {}", redacted_chat_text(text))],
                    0,
                    0,
                    0,
                    "二级好友私聊",
                )));
                let dispatches = self.chat_observations.publish_secondary(
                    frame,
                    "pink",
                    friend_name,
                    false,
                    vec![SecondaryRecognizedMessage {
                        text: text.clone(),
                        sender: None,
                    }],
                )?;
                self.dispatch_chat_observations(dispatches).map(|_| ())
            }
            SecondaryUnreadEffect::Discarded | SecondaryUnreadEffect::NoMessage => Ok(()),
            SecondaryUnreadEffect::SkippedNonFriend => {
                log::warn!("二级监听好友未读: 打开后不是可执行好友会话，跳过 OCR");
                Ok(())
            }
            SecondaryUnreadEffect::Failed(failure) => {
                Err(anyhow!("二级好友未读处理失败：{failure}"))
            }
        }
        .and_then(|_| match outcome.residency() {
            UiResidencyOutcome::Confirmed(UiResidencyTarget::SecondaryCurrentHall) => Ok(()),
            UiResidencyOutcome::Confirmed(other) => {
                Err(anyhow!("二级好友未读驻留结果错误: {other:?}"))
            }
            UiResidencyOutcome::Failed(failure) => {
                Err(anyhow!("二级好友未读后恢复当前大厅失败：{failure}"))
            }
        });

        self.business
            .finish_chat_listener_unread_task(!discard_only)?;
        if result.is_err() {
            self.business.fail_chat_listener_mode_to_primary()?;
            let _ = self.establish_ui_residency(
                UiResidency::Primary,
                ResidencyPurpose::IndependentRecovery("二级未读失败回退一级"),
            );
        }
        result
    }

    pub(super) fn execute_restore_secondary_hall_task(&mut self) -> Result<()> {
        let result = self.establish_ui_residency(
            UiResidency::SecondaryCurrentHall,
            ResidencyPurpose::IndependentRecovery("恢复二级当前大厅任务"),
        );
        self.business.finish_chat_listener_unread_task(false)?;
        match result {
            Ok(()) => Ok(()),
            Err(_) => {
                self.business.fail_chat_listener_mode_to_primary()?;
                let _ = self.establish_ui_residency(
                    UiResidency::Primary,
                    ResidencyPurpose::IndependentRecovery("二级大厅恢复失败回退一级"),
                );
                Err(anyhow!("二级监听无法恢复当前大厅，已回退一级监听"))
            }
        }
    }

    fn secondary_identity_from_frame(&self, image: &DynamicImage) -> Result<SecondaryChatIdentity> {
        let crop = crop_canvas(image, SECONDARY_TITLE_RECT)?;
        let title = self.ocr.merged_text(
            crop,
            self.config.ocr.same_line_y_tolerance,
            OcrPriority::ChatObservation,
        )?;
        log::debug!("二级监听顶部标题 OCR: {}", title);
        Ok(classify_title(&title))
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
        let observation_session = self.chat_observations.begin_exclusive()?;
        let use_secondary = scope == ChatDecisionScope::CurrentHall
            && self.active_ui_residency()? == UiResidency::SecondaryCurrentHall;
        if use_secondary {
            let image = self
                .residency_ui
                .observe(UiResidencyTarget::SecondaryCurrentHall)
                .context("提交二级当前大厅确认基线观察")?
                .wait()
                .context("等待二级当前大厅确认基线观察")?
                .map_err(|failure| anyhow!("建立二级当前大厅确认基线失败：{failure}"))?;
            let previous = secondary_hall_bubbles(&image)?;
            return Ok(ChatDecisionReader {
                kind: ChatDecisionReaderKind::SecondaryCurrentHall { previous },
                screen_lock: DecisionScreenLock::default(),
                _observation_session: observation_session,
            });
        }

        let image = self
            .residency_ui
            .observe(UiResidencyTarget::Primary)
            .context("提交一级聊天确认基线观察")?
            .wait()
            .context("等待一级聊天确认基线观察")?
            .map_err(|failure| anyhow!("建立一级聊天确认基线失败：{failure}"))?;
        let template_args = self.chat_templates.clone();
        let messages = self.scan_chat_with_shared_ocr(&image, &template_args)?;
        Ok(ChatDecisionReader {
            kind: ChatDecisionReaderKind::Primary,
            screen_lock: DecisionScreenLock::from_messages(
                &messages,
                accepts_message_type,
                is_decision,
            ),
            _observation_session: observation_session,
        })
    }

    pub(super) fn poll_chat_decision_reader(
        &self,
        reader: &mut ChatDecisionReader,
    ) -> Result<Vec<ChatMessage>> {
        match &mut reader.kind {
            ChatDecisionReaderKind::Primary => {
                let template_args = self.chat_templates.clone();
                let canvas = Canvas {
                    width: self.config.screen.expected_width,
                    height: self.config.screen.expected_height,
                    resize: true,
                };
                let frame = load_frame(&canvas, &self.game_ui)?;
                self.scan_chat_with_shared_ocr(&frame.image, &template_args)
            }
            ChatDecisionReaderKind::SecondaryCurrentHall { previous } => {
                self.scan_secondary_decision_messages(previous)
            }
        }
    }

    fn scan_secondary_decision_messages(
        &self,
        previous: &mut Vec<SecondaryHallBubble>,
    ) -> Result<Vec<ChatMessage>> {
        let canvas = Canvas {
            width: self.config.screen.expected_width,
            height: self.config.screen.expected_height,
            resize: true,
        };
        let frame = load_frame(&canvas, &self.game_ui)?;
        let current = secondary_hall_bubbles(&frame.image)?;
        match secondary_hall_sequence_delta(Some(previous), &current) {
            SecondaryHallSequenceDelta::RetainedPrefix => {
                log::debug!("二级确认当前只显示旧序列前缀，保留原基线");
                return Ok(Vec::new());
            }
            SecondaryHallSequenceDelta::NoChange => return Ok(Vec::new()),
            SecondaryHallSequenceDelta::EstablishBaseline => unreachable!("baseline is present"),
            SecondaryHallSequenceDelta::LostOverlap | SecondaryHallSequenceDelta::NewFrom(_) => {}
        }

        let Some((refreshed, refreshed_bubbles)) = self.wait_for_secondary_hall_stability()? else {
            log::debug!("二级确认消息尚未稳定，保留旧基线等待下一轮");
            return Ok(Vec::new());
        };
        let start = match secondary_hall_sequence_delta(Some(previous), &refreshed_bubbles) {
            SecondaryHallSequenceDelta::NewFrom(start) => start,
            SecondaryHallSequenceDelta::RetainedPrefix => {
                log::debug!("二级确认稳定观测仍只是旧序列前缀，保留原基线");
                return Ok(Vec::new());
            }
            SecondaryHallSequenceDelta::NoChange => return Ok(Vec::new()),
            SecondaryHallSequenceDelta::EstablishBaseline => unreachable!("baseline is present"),
            SecondaryHallSequenceDelta::LostOverlap => {
                *previous = refreshed_bubbles;
                log::debug!("二级确认气泡稳定后失去重叠，已重建基线");
                return Ok(Vec::new());
            }
        };
        let messages =
            self.recognize_secondary_hall_messages(&refreshed.image, &refreshed_bubbles[start..])?;
        *previous = refreshed_bubbles;
        Ok(messages)
    }

    fn recognize_secondary_hall_messages(
        &self,
        image: &DynamicImage,
        bubbles: &[SecondaryHallBubble],
    ) -> Result<Vec<ChatMessage>> {
        let started = Instant::now();
        let mut messages = Vec::with_capacity(bubbles.len());
        for bubble in bubbles {
            let crop = crop_canvas(image, bubble.rect)?;
            let text = self.ocr.merged_text(
                crop,
                self.config.ocr.same_line_y_tolerance,
                OcrPriority::ChatObservation,
            )?;
            messages.push(ChatMessage {
                message_type: "blue".to_string(),
                block: bubble.rect,
                text,
                visual: rect_chat_change_fingerprint(image, bubble.rect)?,
            });
        }
        let ocr_ms = elapsed_ms(started);
        self.monitor.publish(MonitorEvent::Ocr(OcrSnapshot::new(
            messages.len(),
            messages
                .iter()
                .map(|message| format!("[blue] {}", redacted_chat_text(&message.text)))
                .collect(),
            0,
            ocr_ms,
            ocr_ms,
            "二级当前大厅",
        )));
        Ok(messages)
    }

    fn process_secondary_latest_message(
        &mut self,
        image: &DynamicImage,
        captured_at: Instant,
        message_type: &str,
        friend_name: &str,
    ) -> Result<bool> {
        let Some(rect) = latest_incoming_bubble_rect(image) else {
            return Ok(false);
        };
        Ok(self
            .process_secondary_bubble_rects(
                image,
                captured_at,
                std::iter::once((rect, None)),
                message_type,
                friend_name,
            )?
            .processed)
    }

    fn process_secondary_bubble_rects(
        &mut self,
        image: &DynamicImage,
        captured_at: Instant,
        regions: impl IntoIterator<Item = (Rect, Option<Rect>)>,
        message_type: &str,
        friend_name: &str,
    ) -> Result<SecondaryBubbleProcessOutcome> {
        let started = Instant::now();
        let observation_frame = self.chat_observations.begin_frame(captured_at)?;
        let ocr_started = Instant::now();
        let commands_enabled = self.commands_enabled()?;
        let accepts_turtle_questions = message_type == "blue"
            && commands_enabled
            && self.business.turtle_soup_accepts_questions()?;
        let active_entertainment = if commands_enabled {
            self.business.active_entertainment()?
        } else {
            None
        };
        let observes_hall = message_type == "blue";
        let command_router = ChatCommandRouter::new(&self.custom_workflow);
        let texts = (|| -> Result<Vec<SecondaryOcrMessage>> {
            let mut texts = Vec::new();
            for (rect, sender_rect) in regions {
                let crop = crop_canvas(image, rect)?;
                let text = self.ocr.merged_text(
                    crop,
                    self.config.ocr.same_line_y_tolerance,
                    OcrPriority::ChatObservation,
                )?;
                let classification = if observes_hall && commands_enabled {
                    let routed = secondary_hall_command_text(&text)
                        .and_then(|command_text| {
                            CommandEnvelope::new(
                                &text,
                                SECONDARY_HALL_FALLBACK_SENDER,
                                "blue",
                                command_text,
                                CommandObservation::default(),
                            )
                        })
                        .and_then(|envelope| command_router.route(&envelope, active_entertainment));
                    classify_secondary_hall_message(
                        &text,
                        routed.as_ref().map(|command| &command.command),
                        accepts_turtle_questions,
                    )
                } else if observes_hall {
                    SecondaryHallMessageClassification {
                        kind: SecondaryHallMessageKind::Ignored,
                        requires_sender: false,
                    }
                } else {
                    SecondaryHallMessageClassification {
                        kind: SecondaryHallMessageKind::Command,
                        requires_sender: false,
                    }
                };
                let message_sender = if classification.requires_sender
                    && let Some(sender_rect) = sender_rect
                {
                    let crop = crop_canvas(image, sender_rect)?;
                    Some(normalize_secondary_sender_name(&self.ocr.merged_text(
                        crop,
                        self.config.ocr.same_line_y_tolerance,
                        OcrPriority::ChatObservation,
                    )?))
                } else {
                    None
                };
                texts.push(SecondaryOcrMessage {
                    text,
                    sender: message_sender,
                    kind: classification.kind,
                    requires_sender: classification.requires_sender,
                });
            }
            Ok(texts)
        })();
        let texts = match texts {
            Ok(texts) => texts,
            Err(error) => {
                if let Err(record_error) = self
                    .chat_observations
                    .record_terminal_failure(observation_frame, format!("{error:#}"))
                {
                    log::error!("记录二级聊天观察终止失败异常: {record_error:#}");
                }
                return Err(error);
            }
        };
        let ocr_ms = elapsed_ms(ocr_started);
        self.monitor.publish(MonitorEvent::Ocr(OcrSnapshot::new(
            texts.len(),
            texts
                .iter()
                .map(|message| format!("[{}] {}", message_type, redacted_chat_text(&message.text)))
                .collect(),
            0,
            ocr_ms,
            elapsed_ms(started),
            if message_type == "pink" {
                "二级好友私聊"
            } else {
                "二级当前大厅"
            },
        )));

        if texts.iter().any(|message| {
            message.requires_sender
                && message
                    .sender
                    .as_deref()
                    .is_none_or(|sender| sender.trim().is_empty())
        }) {
            self.chat_observations
                .complete_without_messages(observation_frame)?;
            log::debug!("二级大厅身份相关输入的固定发送者区域 OCR 为空，本轮等待昵称加载");
            return Ok(SecondaryBubbleProcessOutcome {
                processed: false,
                confirmation_pending: true,
            });
        }

        let mut texts = texts
            .into_iter()
            .filter(|message| {
                message_type != "blue" || message.kind != SecondaryHallMessageKind::Ignored
            })
            .collect::<Vec<_>>();
        let turtle_question_indexes = texts
            .iter()
            .enumerate()
            .filter_map(|(index, message)| {
                (message.kind == SecondaryHallMessageKind::TurtleQuestion).then_some(index)
            })
            .collect::<Vec<_>>();
        if !turtle_question_indexes.is_empty() {
            let observations = turtle_question_indexes
                .iter()
                .map(|index| SecondaryOcrObservation {
                    text: texts[*index].text.clone(),
                    player: texts[*index].sender.clone().unwrap_or_default(),
                })
                .collect();
            match self
                .business
                .stabilize_turtle_soup_secondary(observations)?
            {
                SecondaryOcrStability::Pending => {
                    self.chat_observations
                        .complete_without_messages(observation_frame)?;
                    return Ok(SecondaryBubbleProcessOutcome {
                        processed: false,
                        confirmation_pending: true,
                    });
                }
                SecondaryOcrStability::Stable(observations) => {
                    if observations.len() != turtle_question_indexes.len() {
                        return Err(anyhow!(
                            "二级海龟汤 OCR 稳定结果数量不一致: expected={} actual={}",
                            turtle_question_indexes.len(),
                            observations.len()
                        ));
                    }
                    for (index, observation) in
                        turtle_question_indexes.into_iter().zip(observations)
                    {
                        texts[index].text = observation.text;
                        texts[index].sender = Some(observation.player);
                    }
                }
            }
        } else {
            self.business.clear_turtle_soup_secondary_stability()?;
        }

        let messages = texts
            .into_iter()
            .map(|message| SecondaryRecognizedMessage {
                text: message.text,
                sender: message.sender,
            })
            .collect();
        let dispatches = self.chat_observations.publish_secondary(
            observation_frame,
            message_type,
            friend_name,
            accepts_turtle_questions,
            messages,
        )?;
        let processed = self.dispatch_chat_observations(dispatches)?;
        Ok(SecondaryBubbleProcessOutcome {
            processed,
            confirmation_pending: false,
        })
    }

    pub(super) fn process_secondary_chat_observation(
        &self,
        frame: ObservedFrame,
        observation: SecondaryChatObservation,
    ) -> Result<bool> {
        let SecondaryChatObservation {
            message_type,
            friend_name,
            accepts_turtle_questions,
            messages,
        } = observation;
        let mut processed = false;
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
            };
            let shortcut_player = if message_type == "pink" {
                friend_name.trim()
            } else {
                message_sender
                    .as_deref()
                    .map(str::trim)
                    .filter(|sender| !sender.is_empty())
                    .unwrap_or(SECONDARY_HALL_FALLBACK_SENDER)
            };
            if let Some(envelope) = CommandEnvelope::new(
                &text,
                shortcut_player,
                &message_type,
                text.trim(),
                command_observation.clone(),
            ) && envelope.prefix() == CommandPrefix::Hash
            {
                let router = ChatCommandRouter::new(&self.custom_workflow);
                if let Some(parsed) = router.route(&envelope, self.business.active_entertainment()?)
                {
                    self.submit_secondary_command(parsed)?;
                    processed = true;
                    continue;
                }
            }
            if accepts_turtle_questions {
                let question = message_sender
                    .as_deref()
                    .map(str::trim)
                    .filter(|player| !player.is_empty())
                    .and_then(|player| turtle_soup::parse_question_message(&text, Some(player)));
                if let Some(question) = question {
                    processed |= self.handle_turtle_soup_question(question)?;
                    continue;
                }
            }
            let Some(index) = text.find('@') else {
                log::debug!("二级监听气泡不是命令: {}", redacted_chat_text(&text));
                continue;
            };
            let command_text = text[index..].trim().to_string();
            let username = if message_type == "pink" {
                if friend_name.trim().is_empty() {
                    "二级好友"
                } else {
                    friend_name.trim()
                }
            } else {
                message_sender
                    .as_deref()
                    .map(str::trim)
                    .filter(|sender| !sender.is_empty())
                    .unwrap_or(SECONDARY_HALL_FALLBACK_SENDER)
            };
            let Some(envelope) = CommandEnvelope::new(
                &text,
                username,
                &message_type,
                command_text,
                command_observation,
            ) else {
                continue;
            };
            let router = ChatCommandRouter::new(&self.custom_workflow);
            let Some(parsed) = router.route(&envelope, self.business.active_entertainment()?)
            else {
                log::debug!("二级监听气泡未解析为命令");
                continue;
            };
            self.submit_secondary_command(parsed)?;
            processed = true;
        }
        Ok(processed)
    }

    fn wait_for_secondary_bubble_stability(&self) -> Result<Frame> {
        const STABILITY_TIMEOUT_MS: u64 = 500;

        let canvas = Canvas {
            width: self.config.screen.expected_width,
            height: self.config.screen.expected_height,
            resize: true,
        };
        let first = load_frame(&canvas, &self.game_ui)?;
        let mut previous = latest_incoming_fingerprint(&first.image)?;
        let mut latest_frame = first;
        let poll_ms = self
            .config
            .timing
            .chat_scan
            .change_debounce_ms
            .clamp(100, 200);
        let deadline = Instant::now() + Duration::from_millis(STABILITY_TIMEOUT_MS);

        while Instant::now() < deadline {
            sleep(Duration::from_millis(poll_ms));
            let frame = load_frame(&canvas, &self.game_ui)?;
            let current = latest_incoming_fingerprint(&frame.image)?;
            if !secondary_optional_fingerprint_changed(previous.as_ref(), current.as_ref()) {
                return Ok(frame);
            }
            previous = current;
            latest_frame = frame;
        }
        log::debug!("二级监听气泡稳定等待超时，按当前画面继续 OCR");
        Ok(latest_frame)
    }

    fn wait_for_secondary_hall_stability(
        &self,
    ) -> Result<Option<(Frame, Vec<SecondaryHallBubble>)>> {
        let canvas = Canvas {
            width: self.config.screen.expected_width,
            height: self.config.screen.expected_height,
            resize: true,
        };
        let first = load_frame(&canvas, &self.game_ui)?;
        let mut previous = secondary_hall_bubbles(&first.image)?;
        let poll_ms = self
            .config
            .timing
            .chat_scan
            .change_debounce_ms
            .clamp(100, 200);
        let required_samples = self
            .config
            .resolve_stability_count(self.config.stability.secondary_hall_count);
        let mut stable_samples = 1_u32;
        let timeout_ms = poll_ms
            .saturating_mul(u64::from(required_samples.saturating_add(1)))
            .max(500);
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);

        while Instant::now() < deadline {
            sleep(Duration::from_millis(poll_ms));
            let frame = load_frame(&canvas, &self.game_ui)?;
            let current = secondary_hall_bubbles(&frame.image)?;
            if hall_bubble_sequences_stable(&previous, &current) {
                stable_samples = stable_samples.saturating_add(1);
            } else {
                stable_samples = 1;
            }
            if stable_samples >= required_samples {
                return Ok(Some((frame, current)));
            }
            previous = current;
        }
        log::debug!(
            "二级大厅气泡及关联区域稳定等待超时，本轮不进入 OCR: samples={}/{} timeout={}ms",
            stable_samples,
            required_samples,
            timeout_ms
        );
        Ok(None)
    }
}
