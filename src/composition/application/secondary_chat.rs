use super::*;

impl ApplicationRuntime {
    pub(super) fn run_secondary_listener_round(
        &mut self,
        image: &DynamicImage,
        last_friend_bubble: &mut Option<ChangeFingerprint>,
        hall_bubble_sequence: &mut Vec<SecondaryHallBubble>,
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
            *hall_bubble_sequence = secondary_hall_bubbles(image)?;
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
        previous: &mut Vec<SecondaryHallBubble>,
    ) -> Result<bool> {
        let current = secondary_hall_bubbles(image)?;
        if previous.is_empty() {
            self.business.clear_turtle_soup_secondary_stability()?;
            *previous = current;
            log::debug!("二级大厅气泡序列尚未建立，当前仅记录基线");
            return Ok(false);
        }

        let overlap = hall_bubble_sequence_overlap(previous, &current);
        if overlap == 0 {
            self.business.clear_turtle_soup_secondary_stability()?;
            *previous = current;
            log::debug!("二级大厅气泡序列没有可靠重叠，已重建基线，不处理当前可见历史消息");
            return Ok(false);
        }
        if overlap == current.len() {
            self.business.clear_turtle_soup_secondary_stability()?;
            *previous = current;
            return Ok(false);
        }

        let refreshed = self.wait_for_secondary_bubble_stability()?;
        let refreshed_bubbles = secondary_hall_bubbles(&refreshed.image)?;
        let refreshed_overlap = hall_bubble_sequence_overlap(previous, &refreshed_bubbles);
        if refreshed_overlap == 0 {
            self.business.clear_turtle_soup_secondary_stability()?;
            *previous = refreshed_bubbles;
            log::debug!("二级大厅气泡稳定后没有可靠重叠，已重建基线，不处理当前可见历史消息");
            return Ok(false);
        }
        let new_bubbles = &refreshed_bubbles[refreshed_overlap..];
        if new_bubbles.is_empty() {
            self.business.clear_turtle_soup_secondary_stability()?;
            *previous = refreshed_bubbles;
            return Ok(false);
        }

        log::info!(
            "二级大厅检测到 {} 条新增气泡，按显示顺序 OCR",
            new_bubbles.len()
        );
        let outcome = self.process_secondary_bubble_rects(
            &refreshed.image,
            refreshed.captured_at,
            new_bubbles.iter().map(|bubble| bubble.rect),
            "blue",
            "",
        )?;
        if outcome.ocr_pending {
            log::debug!("二级大厅新增气泡的海龟汤 OCR 尚未稳定，保留旧气泡基线等待下轮复核");
            return Ok(false);
        }
        *previous = refreshed_bubbles;
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
            self.establish_ui_residency(
                UiResidency::SecondaryCurrentHall,
                ResidencyPurpose::DecisionObservation("建立二级当前大厅确认基线"),
            )?;
            let canvas = Canvas {
                width: self.config.screen.expected_width,
                height: self.config.screen.expected_height,
                resize: true,
            };
            let frame = load_frame(&canvas, &self.game_ui)?;
            let previous = secondary_hall_bubbles(&frame.image)?;
            return Ok(ChatDecisionReader {
                kind: ChatDecisionReaderKind::SecondaryCurrentHall { previous },
                screen_lock: DecisionScreenLock::default(),
                _observation_session: observation_session,
            });
        }

        self.establish_ui_residency(
            UiResidency::Primary,
            ResidencyPurpose::DecisionObservation("建立一级聊天确认基线"),
        )?;
        let template_args = TemplateArgs::default().resolve(&self.config);
        let canvas = Canvas {
            width: self.config.screen.expected_width,
            height: self.config.screen.expected_height,
            resize: true,
        };
        let frame = load_frame(&canvas, &self.game_ui)?;
        let messages = self.scan_chat_with_shared_ocr(&frame.image, &template_args)?;
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
                let template_args = TemplateArgs::default().resolve(&self.config);
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
        let Some(start) = secondary_new_bubble_start(previous, &current) else {
            *previous = current;
            log::debug!("二级确认气泡序列失去重叠，已重建基线");
            return Ok(Vec::new());
        };
        if start >= current.len() {
            *previous = current;
            return Ok(Vec::new());
        }

        let refreshed = self.wait_for_secondary_bubble_stability()?;
        let refreshed_bubbles = secondary_hall_bubbles(&refreshed.image)?;
        let Some(start) = secondary_new_bubble_start(previous, &refreshed_bubbles) else {
            *previous = refreshed_bubbles;
            log::debug!("二级确认气泡稳定后失去重叠，已重建基线");
            return Ok(Vec::new());
        };
        let rects = refreshed_bubbles[start..]
            .iter()
            .map(|bubble| bubble.rect)
            .collect::<Vec<_>>();
        let messages = self.recognize_secondary_hall_messages(&refreshed.image, &rects)?;
        *previous = refreshed_bubbles;
        Ok(messages)
    }

    fn recognize_secondary_hall_messages(
        &self,
        image: &DynamicImage,
        rects: &[Rect],
    ) -> Result<Vec<ChatMessage>> {
        let started = Instant::now();
        let mut messages = Vec::with_capacity(rects.len());
        for rect in rects {
            let crop = crop_canvas(image, *rect)?;
            let text = self.ocr.merged_text(
                crop,
                self.config.ocr.same_line_y_tolerance,
                OcrPriority::ChatObservation,
            )?;
            messages.push(ChatMessage {
                message_type: "blue".to_string(),
                block: *rect,
                text,
                visual: rect_chat_change_fingerprint(image, *rect)?,
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
                std::iter::once(rect),
                message_type,
                friend_name,
            )?
            .processed)
    }

    fn process_secondary_bubble_rects(
        &mut self,
        image: &DynamicImage,
        captured_at: Instant,
        rects: impl IntoIterator<Item = Rect>,
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
        let captures_hash_sender = message_type == "blue" && commands_enabled;
        let texts = (|| -> Result<Vec<(Rect, String, Option<String>)>> {
            let mut texts = Vec::new();
            for rect in rects {
                let crop = crop_canvas(image, rect)?;
                let text = self.ocr.merged_text(
                    crop,
                    self.config.ocr.same_line_y_tolerance,
                    OcrPriority::ChatObservation,
                )?;
                let trimmed_text = text.trim_start();
                let starts_with_hash =
                    trimmed_text.starts_with('#') || trimmed_text.starts_with('＃');
                let message_sender = if captures_hash_sender && starts_with_hash {
                    let sender_rect = secondary_message_sender_rect(image, rect);
                    let crop = crop_canvas(image, sender_rect)?;
                    Some(self.ocr.merged_text(
                        crop,
                        self.config.ocr.same_line_y_tolerance,
                        OcrPriority::ChatObservation,
                    )?)
                } else {
                    None
                };
                texts.push((rect, text, message_sender));
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
                .map(|(_, text, _)| format!("[{}] {}", message_type, redacted_chat_text(text)))
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

        let texts = if accepts_turtle_questions {
            let observations = texts
                .into_iter()
                .map(|(_, text, message_sender)| SecondaryOcrObservation {
                    text,
                    player: message_sender.unwrap_or_default(),
                })
                .collect::<Vec<_>>();
            match self
                .business
                .stabilize_turtle_soup_secondary(observations)?
            {
                SecondaryOcrStability::Pending => {
                    self.chat_observations
                        .complete_without_messages(observation_frame)?;
                    return Ok(SecondaryBubbleProcessOutcome {
                        processed: false,
                        ocr_pending: true,
                    });
                }
                SecondaryOcrStability::Stable(observations) => observations
                    .into_iter()
                    .map(|observation| (observation.text, Some(observation.player)))
                    .collect::<Vec<_>>(),
            }
        } else {
            self.business.clear_turtle_soup_secondary_stability()?;
            texts
                .into_iter()
                .map(|(_, text, message_sender)| (text, message_sender))
                .collect::<Vec<_>>()
        };

        let messages = texts
            .into_iter()
            .map(|(text, sender)| SecondaryRecognizedMessage { text, sender })
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
            ocr_pending: false,
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
                message_sender.as_deref().map(str::trim).unwrap_or_default()
            };
            if !shortcut_player.is_empty() {
                let synthetic = if message_type == "pink" {
                    format!("[{}]：{}", shortcut_player, text.trim())
                } else {
                    format!("{}：{}", shortcut_player, text.trim())
                };
                if let Some(parsed) = command::parse_entertainment_shortcut(
                    &synthetic,
                    &message_type,
                    self.business.active_entertainment()?,
                ) {
                    self.submit_secondary_command(parsed, command_observation.clone())?;
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
            let command_text = text[index..].trim();
            let synthetic = if message_type == "pink" {
                let username = if friend_name.trim().is_empty() {
                    "二级好友"
                } else {
                    friend_name.trim()
                };
                format!("[{}]：{}", username, command_text)
            } else {
                format!("二级大厅：{}", command_text)
            };
            let parsed = command::parse_text(&synthetic, &message_type).or_else(|| {
                self.custom_workflow
                    .parse_chat(&synthetic, &message_type)
                    .map(from_custom_workflow_match)
            });
            let Some(parsed) = parsed else {
                log::debug!("二级监听气泡未解析为命令");
                continue;
            };
            self.submit_secondary_command(parsed, command_observation)?;
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
}
