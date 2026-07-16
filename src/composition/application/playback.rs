use super::*;

impl ApplicationRuntime {
    pub(super) fn execute_advance_queue_task(&mut self, reason: &'static str) -> Result<()> {
        self.consume_queue(reason)
    }

    pub(super) fn run_playback_monitor_loop(&mut self) {
        let tick_ms = self.config.timing.playback.monitor_tick_ms.max(50);
        let status_ms = self.config.timing.playback.monitor_status_ms.max(tick_ms);
        let mut snapshot: Option<PlaybackSnapshot> = None;
        let mut next_status_at = Instant::now();
        while self.running.load(AtomicOrdering::SeqCst) {
            if self.paused.load(AtomicOrdering::SeqCst) {
                sleep(Duration::from_millis(tick_ms));
                continue;
            }
            let now = Instant::now();
            if snapshot.is_none() || now >= next_status_at {
                match self.player.status() {
                    Ok(status) => {
                        snapshot = Some(PlaybackSnapshot {
                            status,
                            captured_at: now,
                        });
                        next_status_at = now + Duration::from_millis(status_ms);
                    }
                    Err(error) => {
                        log::error!("播放监控状态查询失败: {error:#}");
                        snapshot = None;
                        next_status_at = now + Duration::from_millis(status_ms);
                    }
                }
            }
            if let Some(playback_snapshot) = snapshot.as_ref() {
                match self.handle_playback_monitor_snapshot(playback_snapshot) {
                    Ok(true) => {
                        if let Ok(status) = self.player.status() {
                            snapshot = Some(PlaybackSnapshot {
                                status,
                                captured_at: Instant::now(),
                            });
                        } else {
                            snapshot = None;
                        }
                        next_status_at = Instant::now() + Duration::from_millis(status_ms);
                    }
                    Ok(false) => {}
                    Err(error) => {
                        log::error!("播放监控处理失败: {error:#}");
                        next_status_at = Instant::now() + Duration::from_millis(status_ms);
                    }
                }
            }
            sleep(Duration::from_millis(tick_ms));
        }
    }

    fn handle_playback_monitor_snapshot(&mut self, snapshot: &PlaybackSnapshot) -> Result<bool> {
        let scheduler = self.business.scheduler_snapshot()?;
        let context = QueueAdvanceContext {
            queue_empty: self.playback_queue()?.is_empty(),
            has_pending_playback_task: scheduler.pending_playback_related(),
            command_executing: scheduler.is_busy(),
            song_command_executing: scheduler.active_playback_related(),
        };
        let decision = self
            .player
            .maybe_advance_queue(estimated_player_status(snapshot), context)?;
        self.update_monitor_playback_controller();
        match decision {
            QueueAdvanceDecision::None => Ok(false),
            QueueAdvanceDecision::PlaybackStateChanged
            | QueueAdvanceDecision::PauseForQueue
            | QueueAdvanceDecision::ResumeIfIdle => Ok(true),
            QueueAdvanceDecision::AdvanceQueue { reason } => {
                self.push_pending_task(PendingTask::AdvanceQueue { reason })?;
                Ok(true)
            }
        }
    }

    pub(super) fn maybe_warn_hall_expiring(&mut self) -> Result<bool> {
        if !self.executor_is_idle()? {
            return Ok(false);
        }
        let minutes = {
            let state = self.business.runtime_state_snapshot()?;
            if state.hall_expiring_warning_sent {
                return Ok(false);
            }
            let Some(minutes) = state.hall_remaining_minutes_now() else {
                return Ok(false);
            };
            if minutes > HALL_EXPIRING_WARNING_MINUTES {
                return Ok(false);
            }
            minutes
        };
        let message = if minutes == 0 {
            "大厅即将到期".to_string()
        } else {
            format!("大厅即将到期，剩余{}分钟", minutes)
        };
        self.reply(&message)?;
        self.business
            .patch_runtime_state(crate::features::playback::RuntimeStatePatch {
                hall_expiring_warning_sent: Some(true),
                ..crate::features::playback::RuntimeStatePatch::default()
            })?;
        Ok(true)
    }

    pub(super) fn clear_hall_countdown_cache_for_new_visual_session(
        &self,
        reason: &str,
    ) -> Result<bool> {
        let cleared = self.business.clear_hall_countdown_cache()?;
        let visual_session = self.chat_observations.begin_visual_session()?;
        if cleared {
            log::info!("{reason}，已清理大厅倒计时缓存，等待本次大厅检测重新确认");
        }
        log::info!("{reason}，聊天观察进入新视觉会话: {}", visual_session.get());
        Ok(cleared)
    }

    pub(super) fn play_request_confirmed(
        &mut self,
        request: &ResolvedSongRequest,
        allow_switch_source: bool,
    ) -> Result<PlaybackOutcome> {
        if self.song_dedup_limited(request)? {
            log::info!(
                "长时间同歌去重拦截: keyword={} uri={}",
                request.keyword,
                request.uri
            );
            self.reply(&self.song_dedup_reject_message(request))?;
            return Ok(PlaybackOutcome::DedupLimited);
        }
        if request.uri.trim().is_empty() {
            let source = if request.source.trim().is_empty() {
                "qqmusic"
            } else {
                &request.source
            };
            let picked = match classify_player_search(self.player_search.search_and_pick(
                &request.keyword,
                source,
                request.prefer_accompaniment,
            )) {
                PlayerSearchResolution::Found(picked) => picked,
                PlayerSearchResolution::Failed(error) => {
                    self.report_player_search_failure(
                        &request_label(request),
                        "点歌搜索失败",
                        &error,
                    )?;
                    return Ok(PlaybackOutcome::Error);
                }
                PlayerSearchResolution::NoSource => {
                    self.reply("平台无对应歌曲音源")?;
                    return Ok(PlaybackOutcome::NoSource);
                }
            };
            log::info!(
                "播放器候选: {} -> {}",
                picked.candidate.text,
                picked.candidate.uri
            );
            let mut resolved = request.clone();
            resolved.keyword = picked.candidate.text;
            resolved.source = source.to_string();
            resolved.uri = picked.candidate.uri;
            return self.play_request_confirmed(&resolved, allow_switch_source);
        }
        let playback_request = self.playback_request_from_resolved(request);
        self.play_playback_request(&playback_request, allow_switch_source, false)
    }

    fn play_playback_request(
        &mut self,
        request: &PlaybackRequest,
        allow_switch_source: bool,
        confirm_after_switch: bool,
    ) -> Result<PlaybackOutcome> {
        let mut attempt = match self.player.play_request_uri(request) {
            Ok(attempt) => attempt,
            Err(error) => {
                let message = error.to_string();
                log::error!("播放候选失败: {message}");
                self.reply(if message.trim().is_empty() {
                    "平台无对应歌曲音源"
                } else {
                    message.trim()
                })?;
                return Ok(PlaybackOutcome::Error);
            }
        };
        self.complete_playback_verification(
            request,
            &mut attempt,
            allow_switch_source,
            confirm_after_switch,
        )
    }

    fn complete_playback_verification(
        &mut self,
        request: &PlaybackRequest,
        attempt: &mut PlaybackAttempt,
        allow_switch_source: bool,
        confirm_after_switch: bool,
    ) -> Result<PlaybackOutcome> {
        match self.player.verify_playback_started(request, attempt)? {
            PlaybackVerification::Success { status, message } => {
                if confirm_after_switch {
                    match self.confirm_switched_source_result(&status)? {
                        UserDecision::Skip => {
                            self.player.reject_mismatch_as_no_source(Some(&status))?;
                            self.report_no_source(Some(&status), false)?;
                            self.update_monitor_playback_controller();
                            return Ok(PlaybackOutcome::NoSource);
                        }
                        UserDecision::Stopped => return Ok(PlaybackOutcome::Error),
                        _ => {}
                    }
                }
                self.reply(&message)?;
                self.update_monitor_playback_controller();
                Ok(PlaybackOutcome::Success)
            }
            PlaybackVerification::NoSource => {
                self.report_no_source(None, false)?;
                self.update_monitor_playback_controller();
                Ok(PlaybackOutcome::NoSource)
            }
            PlaybackVerification::MismatchedCandidate(mismatch) => {
                match self.handle_playback_mismatch(
                    request,
                    &mismatch.status,
                    &mismatch.local_reason,
                    allow_switch_source,
                )? {
                    MismatchDecision::NoSource => {
                        self.player
                            .reject_mismatch_as_no_source(Some(&mismatch.status))?;
                        self.report_no_source(Some(&mismatch.status), false)?;
                        self.update_monitor_playback_controller();
                        Ok(PlaybackOutcome::NoSource)
                    }
                    MismatchDecision::SwitchSource => self.switch_source_and_play(
                        &request.keyword,
                        &request.source,
                        request.prefer_accompaniment,
                    ),
                }
            }
        }
    }

    fn handle_playback_mismatch(
        &mut self,
        request: &PlaybackRequest,
        status: &PlayerStatus,
        local_reason: &str,
        allow_switch_source: bool,
    ) -> Result<MismatchDecision> {
        log::info!("歌曲暂不匹配: {}", local_reason);
        if request.uri.trim().is_empty() || status.current_uri.trim().is_empty() {
            log::info!("播放确认缺少 URI，拒绝使用歌名或歌手兜底");
            return Ok(MismatchDecision::NoSource);
        }
        if allow_switch_source {
            Ok(MismatchDecision::SwitchSource)
        } else {
            Ok(MismatchDecision::NoSource)
        }
    }

    fn switch_source_and_play(
        &mut self,
        keyword: &str,
        current_source: &str,
        prefer_accompaniment: bool,
    ) -> Result<PlaybackOutcome> {
        let next_source = if current_source == "netease" {
            "qqmusic"
        } else {
            "netease"
        };
        let label = if next_source == "netease" {
            "网易"
        } else {
            "QQ"
        };
        self.reply(&format!("换源到{}: {}", label, keyword))?;
        let request = ResolvedSongRequest {
            keyword: keyword.to_string(),
            source: next_source.to_string(),
            prefer_accompaniment,
            ai_original_text: String::new(),
            uri: String::new(),
            friend_username: String::new(),
            console_bypass_dedup: false,
        };
        let outcome = self.play_request_confirmed(&request, false)?;
        if outcome == PlaybackOutcome::Success
            && let Ok(status) = self.player.status()
            && matches!(
                self.confirm_switched_source_result(&status)?,
                UserDecision::Skip
            )
        {
            self.player.reject_mismatch_as_no_source(Some(&status))?;
            self.report_no_source(Some(&status), false)?;
            return Ok(PlaybackOutcome::NoSource);
        }
        Ok(outcome)
    }

    fn confirm_switched_source_result(&mut self, status: &PlayerStatus) -> Result<UserDecision> {
        let message = format!(
            "换源结果:{},@确认@跳过",
            song_title(&status.name, &status.singer)
        );
        if self.reply(&message).is_err() {
            return Ok(UserDecision::Timeout);
        }
        self.wait_for_decision(false, false, true)
    }

    pub(super) fn wait_for_decision(
        &mut self,
        allow_switch_source: bool,
        allow_ai: bool,
        timeout_confirms: bool,
    ) -> Result<UserDecision> {
        let accepts_message_type = |message_type: &str| message_type == "blue";
        let is_decision = |text: &str| parse_decision_command(text).is_some();
        let mut reader = self.begin_chat_decision_reader(
            ChatDecisionScope::CurrentHall,
            &accepts_message_type,
            &is_decision,
        )?;
        let timeout = Duration::from_millis(self.config.timing.decision.timeout_ms);
        let map_web_decision = |decision| match decision {
            DecisionAction::Confirm => UserDecision::Confirm,
            DecisionAction::Skip => UserDecision::Skip,
            DecisionAction::SwitchSource => UserDecision::SwitchSource,
            DecisionAction::Ai => UserDecision::Ai,
        };
        let web_decision =
            self.business
                .begin_decision("点歌候选确认", allow_switch_source, allow_ai, timeout)?;
        let deadline = Instant::now() + timeout;
        while self.running.load(AtomicOrdering::SeqCst) && Instant::now() < deadline {
            let wait =
                Duration::from_millis(reader.poll_interval_ms(self.config.timing.decision.poll_ms))
                    .min(deadline.saturating_duration_since(Instant::now()));
            if let Some(decision) = web_decision.wait(wait)? {
                return Ok(map_web_decision(decision));
            }
            let messages = match self.poll_chat_decision_reader(&mut reader) {
                Ok(messages) => messages,
                Err(error) => {
                    log::error!("确认命令扫描失败: {error:#}");
                    continue;
                }
            };
            for message in messages {
                if message.message_type != "blue"
                    || message.text.is_empty()
                    || is_decision_feedback_text(&message.text)
                {
                    continue;
                }
                let Some(decision) = parse_decision_command(&message.text) else {
                    continue;
                };
                if !reader.accept_once(&message) {
                    continue;
                }
                match decision {
                    UserDecision::Confirm => return Ok(UserDecision::Confirm),
                    UserDecision::Skip => return Ok(UserDecision::Skip),
                    UserDecision::SwitchSource if allow_switch_source => {
                        return Ok(UserDecision::SwitchSource);
                    }
                    UserDecision::Ai if allow_ai => return Ok(UserDecision::Ai),
                    _ => {}
                }
            }
        }
        if let Some(decision) = web_decision.wait(Duration::from_millis(0))? {
            return Ok(map_web_decision(decision));
        }
        if !self.running.load(AtomicOrdering::SeqCst) {
            Ok(UserDecision::Stopped)
        } else if timeout_confirms {
            Ok(UserDecision::Timeout)
        } else {
            self.reply(if allow_switch_source {
                "此平台匹配失败,命令已超时(20s)下次可以尝试@确认@跳过@换源"
            } else {
                "此平台匹配失败,命令已超时(20s)下次可以尝试@确认@跳过"
            })?;
            Ok(UserDecision::Timeout)
        }
    }

    fn report_no_source(&self, status: Option<&PlayerStatus>, pause_playback: bool) -> Result<()> {
        if pause_playback && status.is_some_and(|status| status.status == "playing") {
            let _ = self.player.reject_mismatch_as_no_source(status);
            self.update_monitor_playback_controller();
        }
        self.reply("平台无对应歌曲音源")
    }

    pub(super) fn consume_queue(&mut self, reason: &str) -> Result<()> {
        loop {
            let Some(item) = self.playback_queue()?.into_iter().next() else {
                return Ok(());
            };
            log::info!("消费队列({}): {}", reason, item.keyword);
            let request = ResolvedSongRequest {
                keyword: item.keyword.clone(),
                source: item.source.clone(),
                prefer_accompaniment: item.prefer_accompaniment,
                ai_original_text: item.ai_original_text.clone(),
                uri: item.uri.clone(),
                friend_username: item.friend_username.clone(),
                console_bypass_dedup: item.dedup_bypass,
            };
            if self.song_dedup_limited(&request)? {
                let _ = self
                    .business
                    .remove_playback_queue(QueueRemoval::Id(item.id))?;
                log::info!("队列项近期已播放过，已跳过: {}", item.keyword);
                self.reply(&self.song_dedup_skip_message(&request))?;
                continue;
            }
            let outcome = self.play_request_confirmed(&request, true)?;
            match outcome {
                PlaybackOutcome::Success => {
                    let _ = self
                        .business
                        .remove_playback_queue(QueueRemoval::Id(item.id))?;
                    return Ok(());
                }
                PlaybackOutcome::NoSource => {
                    let _ = self
                        .business
                        .remove_playback_queue(QueueRemoval::Id(item.id))?;
                    log::error!("队列项无音源，已丢弃: {}", item.keyword);
                }
                PlaybackOutcome::Error => {
                    log::error!("队列项播放失败，保留在队首: {}", item.keyword);
                    return Ok(());
                }
                PlaybackOutcome::DedupLimited => {
                    let _ = self
                        .business
                        .remove_playback_queue(QueueRemoval::Id(item.id))?;
                    log::info!("队列项近期已播放过，已跳过: {}", item.keyword);
                }
            }
        }
    }
}
