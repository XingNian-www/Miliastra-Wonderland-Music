use super::*;

use std::collections::HashSet;

use miliastra_playback::PlayableTrack;

use crate::features::playback::{
    BackgroundLyricsScope, PlaybackCommandContext, PlaybackCommandPort, PlaybackExecutionPort,
    PlaybackPickedCandidate, PlaybackResult, PlaybackSearchFailure, PlaybackVerification,
    QueueRemoval,
};

fn immediate_playback_evidence(
    command: &PlaybackCommand,
    outcome: &anyhow::Result<()>,
) -> (&'static str, String) {
    match outcome {
        Ok(()) => ("completed", command.lock_key()),
        Err(_) => (
            "failed",
            format!(
                "handler_error; effects_may_be_partial: {}",
                command.lock_key()
            ),
        ),
    }
}

impl ApplicationRuntime {
    pub(super) fn stop_background_lyrics_output(&self) -> Result<bool> {
        self.business.background_commands.stop("lyrics")
    }

    pub(super) fn execute_playback_intent(
        &mut self,
        parsed: &RoutedCommand,
        command: &PlaybackCommand,
    ) -> Result<()> {
        let context = PlaybackCommandContext {
            message_type: parsed.message_type.clone(),
            username: command_username(parsed).to_string(),
            user_command: parsed.user_command.clone(),
        };
        let outcome = self
            .playback
            .playback_application
            .clone()
            .execute_command(&context, command, self);
        // Playback commands are synchronous, but a reply/logging error may follow a
        // player-side effect. Record the dispatch either way; failed means the handler
        // returned an error, whose side effects may be partial.
        let (status, result) = immediate_playback_evidence(command, &outcome);
        crate::debug_read_only::record_command_immediate_with_status(
            parsed,
            "immediatePlayback",
            status,
            result,
        );
        outcome
    }

    pub(super) fn execute_advance_queue_task(&mut self, reason: &'static str) -> Result<()> {
        if !automatic_advance_allowed(&self.lifecycle.live_configs) {
            log::info!("配置重载待处理，跳过自动下一曲: reason={reason}");
            return Ok(());
        }
        self.playback
            .playback_application
            .clone()
            .consume_queue_after_monitor(reason, self)
            .map(|_| ())
    }

    pub(super) fn play_request_confirmed(
        &mut self,
        request: &ResolvedSongRequest,
    ) -> Result<PlaybackResult> {
        let selection = request.playback_selection();
        let result: PlaybackResult = self
            .playback
            .playback_application
            .clone()
            .play_confirmed(&selection, self)?;
        log::info!(
            "播放流程完成: outcome={:?} requested_uri={} final_uri={} actual_uri={} source_switched={} reason={}",
            result.outcome(),
            result.requested().uri(),
            result.final_request().uri(),
            result
                .status()
                .map(|status| status.current_uri.as_str())
                .unwrap_or_default(),
            result.source_switched(),
            result.failure_reason().unwrap_or_default()
        );
        Ok(result)
    }

    /// allow_local 为 false 时 @本地 与未开放的 @换源/@AI 一样被忽略，
    /// 不会中断本次点歌，而是继续等待其他确认命令。
    pub(super) fn prompt_and_wait_for_decision_batch(
        &mut self,
        messages: &[String],
        allow_switch_source: bool,
        allow_ai: bool,
        allow_local: bool,
        timeout_confirms: bool,
    ) -> Result<SongRequestDecision> {
        let reader = self.begin_song_decision_reader()?;
        self.reply_batch(messages, 0)?;
        self.wait_for_decision_with_reader(
            reader,
            allow_switch_source,
            allow_ai,
            allow_local,
            timeout_confirms,
        )
    }

    fn begin_song_decision_reader(&self) -> Result<ChatDecisionReader> {
        let accepts_message_type = |message_type: &str| message_type == "blue";
        let is_decision = |text: &str| SongRequestDecision::parse(text).is_some();
        self.begin_chat_decision_reader(
            ChatDecisionScope::CurrentHall,
            &accepts_message_type,
            &is_decision,
        )
    }

    fn wait_for_decision_with_reader(
        &mut self,
        mut reader: ChatDecisionReader,
        allow_switch_source: bool,
        allow_ai: bool,
        allow_local: bool,
        timeout_confirms: bool,
    ) -> Result<SongRequestDecision> {
        let timeout = Duration::from_millis(self.lifecycle.config.timing.decision.timeout_ms);
        let map_web_decision = |decision| match decision {
            DecisionAction::Confirm => SongRequestDecision::Confirm,
            DecisionAction::Skip => SongRequestDecision::Skip,
            DecisionAction::SwitchSource => SongRequestDecision::SwitchSource,
            DecisionAction::Ai => SongRequestDecision::Ai,
        };
        let web_decision = self.business.business.begin_decision(
            "点歌候选确认",
            allow_switch_source,
            allow_ai,
            timeout,
        )?;
        let deadline = Instant::now() + timeout;
        while self.lifecycle.running.load(AtomicOrdering::SeqCst) && Instant::now() < deadline {
            let messages = self.wait_for_chat_decision_messages_with_ids(
                &mut reader,
                self.lifecycle.config.timing.decision.poll_ms,
                deadline.saturating_duration_since(Instant::now()),
            )?;
            if !self.lifecycle.running.load(AtomicOrdering::SeqCst) {
                return Ok(SongRequestDecision::Stopped);
            }
            if let Some(decision) = web_decision.wait(Duration::ZERO)? {
                return Ok(map_web_decision(decision));
            }
            for (message, message_id) in messages {
                if message.message_type != "blue"
                    || message.text.is_empty()
                    || SongRequestDecision::is_feedback_text(&message.text)
                {
                    continue;
                }
                let Some(decision) = SongRequestDecision::parse(&message.text) else {
                    log::info!(
                        "命令未执行：点歌确认窗口未识别为决策 text={}",
                        redacted_chat_text(&message.text)
                    );
                    continue;
                };
                if !reader.accept_once(&message) {
                    log::info!(
                        "命令未执行：点歌决策被基线或已消费否决 text={}",
                        redacted_chat_text(&message.text)
                    );
                    continue;
                }
                // 消费成功即确认 business：独占窗口回放时主链路不再把它当成新增追加，
                // 避免对已被决策读者正常消费的命令误报“未执行”。
                self.ui.chat_timeline.acknowledge_business(&message_id)?;
                // 聊天侧已作出决定：立即关闭 Web 决策，后续 submit 直接失败而不是假成功。
                web_decision.close();
                match decision {
                    SongRequestDecision::Confirm => return Ok(SongRequestDecision::Confirm),
                    SongRequestDecision::Skip => return Ok(SongRequestDecision::Skip),
                    SongRequestDecision::SwitchSource if allow_switch_source => {
                        return Ok(SongRequestDecision::SwitchSource);
                    }
                    SongRequestDecision::Ai if allow_ai => return Ok(SongRequestDecision::Ai),
                    SongRequestDecision::LocalLibrary if allow_local => return Ok(decision),
                    SongRequestDecision::Select | SongRequestDecision::SelectIndex(_) => {
                        return Ok(decision);
                    }
                    _ => {
                        log::info!(
                            "命令未执行：点歌决策动作未开放 decision={:?} allow_switch_source={} allow_ai={} allow_local={} text={}",
                            decision,
                            allow_switch_source,
                            allow_ai,
                            allow_local,
                            redacted_chat_text(&message.text)
                        );
                    }
                }
            }
        }
        if let Some(decision) = web_decision.wait(Duration::from_millis(0))? {
            return Ok(map_web_decision(decision));
        }
        if !self.lifecycle.running.load(AtomicOrdering::SeqCst) {
            Ok(SongRequestDecision::Stopped)
        } else if timeout_confirms {
            Ok(SongRequestDecision::Timeout)
        } else {
            Ok(SongRequestDecision::Cancelled)
        }
    }
}

/// 待处理的子进程重载优先于值守器安排的所有队列推进。
/// 当前请求进入终态后，队列仍会持久化并由替代进程继续消费。
fn automatic_advance_allowed(live_configs: &crate::config::LiveConfigs) -> bool {
    !live_configs.has_pending_reload()
}

impl PlaybackExecutionPort for ApplicationRuntime {
    fn reply(&mut self, message: &str) -> Result<()> {
        ApplicationRuntime::reply(self, message)
    }

    fn render_reply(
        &mut self,
        spec: &crate::features::reply_template::ReplyTemplate,
        vars: &crate::features::reply_template::ReplyVars,
    ) -> String {
        // features 层只声明模板；覆盖文案在组装层查，业务模块不依赖配置中心。
        // 这里必须与 reply_template 共用同一条路径：队列分批消息只调 render_reply，
        // 若覆盖只写在 reply_template 里，这些消息就会绕过用户配置。
        let override_text = self
            .lifecycle
            .live_configs
            .replies
            .read()
            .ok()
            .and_then(|config| config.get(spec.key).map(str::to_string));
        let rendered =
            crate::features::reply_template::render(spec, override_text.as_deref(), vars);
        if !rendered.missing.is_empty() {
            log::warn!("回复模板 {} 缺少变量 {:?}", spec.key, rendered.missing);
        }
        rendered.text
    }

    fn reply_template(
        &mut self,
        spec: &crate::features::reply_template::ReplyTemplate,
        vars: &crate::features::reply_template::ReplyVars,
    ) -> Result<()> {
        let text = self.render_reply(spec, vars);
        ApplicationRuntime::reply(self, &text)
    }

    fn update_monitor(&mut self) {
        self.update_monitor_playback_controller();
    }

    fn search_and_pick(
        &mut self,
        keyword: &str,
        source: &str,
        prefer_accompaniment: bool,
    ) -> std::result::Result<Option<PlaybackPickedCandidate>, PlaybackSearchFailure> {
        self.playback
            .player_search
            .search_and_pick(keyword, source, prefer_accompaniment)
            .map(|picked| {
                picked.map(|picked| PlaybackPickedCandidate {
                    track: picked.candidate.playable_track(),
                    text: picked.candidate.text,
                    candidate_snapshot: picked.candidate_snapshot,
                })
            })
            .map_err(playback_search_failure)
    }

    fn song_dedup_limited(&mut self, request: &PlaybackRequest) -> Result<bool> {
        self.playback.player.song_dedup_limited(request)
    }

    fn play_and_verify(&mut self, request: &PlaybackRequest) -> Result<PlaybackVerification> {
        self.playback.player.play_and_verify(request)
    }

    fn is_track_unavailable_error(&self, error: &anyhow::Error) -> bool {
        self.playback.player.is_track_unavailable_error(error)
    }

    fn item_scoped_playback_error_message(&self, error: &anyhow::Error) -> Option<&'static str> {
        self.playback
            .player
            .item_scoped_playback_error_message(error)
    }

    fn player_status(&mut self) -> Result<PlayerStatus> {
        self.playback.player.status()
    }

    fn playback_queue(&mut self) -> Result<Vec<QueueItem>> {
        ApplicationRuntime::playback_queue(self)
    }

    fn enqueue_playlist_item(&mut self, item: QueueItem) -> Result<bool> {
        self.business
            .business
            .push_playback_queue(item)
            .map(|outcome| outcome.accepted)
            .map_err(anyhow::Error::from)
    }

    fn remove_playlist_auto_items(&mut self) -> Result<()> {
        // 走队列内原子 retain + 落盘，避免「取快照索引 -> 按索引删」之间的并发插入错位。
        self.business
            .business
            .remove_playlist_auto_items()
            .map_err(anyhow::Error::from)?;
        Ok(())
    }

    fn pick_playback_pool_track(
        &mut self,
        excluded: &HashSet<miliastra_playback::TrackKey>,
    ) -> Result<Option<PlayableTrack>> {
        self.business
            .business
            .pick_playback_pool_track(excluded)
            .map_err(anyhow::Error::from)
    }

    fn remove_playback_queue(&mut self, removal: QueueRemoval) -> Result<()> {
        self.business
            .business
            .remove_playback_queue(removal)
            .map(|_| ())
            .map_err(anyhow::Error::from)
    }

    fn user_pause_active(&mut self) -> Result<bool> {
        self.playback.player.user_pause_active()
    }

    fn automatic_queue_advance_allowed(&mut self) -> Result<bool> {
        Ok(automatic_advance_allowed(&self.lifecycle.live_configs))
    }

    fn preload_track(&mut self, track: &miliastra_playback::PlayableTrack) -> Result<()> {
        self.playback
            .native_playback
            .preload(track.clone())
            .map_err(anyhow::Error::from)
    }
}

impl PlaybackCommandPort for ApplicationRuntime {
    fn reply_batch(&mut self, messages: &[String], delay_ms: u64) -> Result<()> {
        let refs = messages.iter().map(String::as_str).collect::<Vec<_>>();
        ApplicationRuntime::reply_batch(self, &refs, delay_ms)
    }

    fn log_executed(
        &mut self,
        context: &PlaybackCommandContext,
        final_command: &str,
    ) -> Result<()> {
        self.log_executed_command_fields(
            &context.message_type,
            &context.username,
            &context.user_command,
            final_command,
        )
    }

    fn pause_by_user(&mut self) -> Result<String> {
        self.playback.player.pause_by_user()
    }

    fn resume_by_user(&mut self) -> Result<String> {
        self.playback.player.resume_by_user()
    }

    fn previous_playback_request(
        &mut self,
    ) -> Result<Option<crate::features::playback::PlaybackRequest>> {
        self.playback.player.previous_playback_request()
    }

    fn set_volume(&mut self, volume: &str) -> Result<()> {
        self.playback.player.set_volume(volume).map(|_| ())
    }

    fn remove_playback_pool_track(&mut self, key: &miliastra_playback::TrackKey) -> Result<bool> {
        self.business
            .business
            .remove_playback_pool_track(key.clone())
            .map_err(anyhow::Error::from)
    }

    fn toggle_lyrics(&mut self) -> Result<String> {
        self.playback.player.toggle_lyrics()
    }

    fn remove_playback_queue_indexes(
        &mut self,
        indexes: Vec<usize>,
    ) -> Result<Vec<(usize, QueueItem)>> {
        self.business
            .business
            .remove_playback_queue_indexes(indexes)
            .map_err(anyhow::Error::from)
    }

    fn clear_playback_queue(&mut self) -> Result<usize> {
        self.business
            .business
            .clear_playback_queue()
            .map_err(anyhow::Error::from)
    }

    fn should_stop_continuous_lyrics(&mut self) -> Result<bool> {
        if !self.lifecycle.running.load(AtomicOrdering::SeqCst) {
            return Ok(true);
        }
        if self
            .business
            .business
            .hall_state_snapshot()?
            .remaining_minutes_now()
            == Some(0)
        {
            return Ok(true);
        }
        Ok(!self
            .business
            .task_engine
            .snapshot()
            .map_err(anyhow::Error::from)?
            .pending_labels()
            .is_empty())
    }

    fn start_background_lyrics(
        &mut self,
        duration: Option<Duration>,
        scope: BackgroundLyricsScope,
    ) -> Result<bool> {
        workers::start_background_lyrics(
            &self.business.background_commands,
            self.playback.player.clone(),
            self.business.task_engine.clone(),
            self.lifecycle.running.clone(),
            Duration::from_millis(
                *self
                    .lifecycle
                    .live_configs
                    .monitor_status_ms
                    .read()
                    .expect("播放状态校准间隔共享锁已中毒"),
            ),
            duration,
            scope,
        )
    }

    fn stop_background_lyrics(&mut self) -> Result<bool> {
        self.stop_background_lyrics_output()
    }

    fn wait(&mut self, duration: Duration) {
        sleep(duration);
    }
}

fn playback_search_failure(error: PlayerSearchClientError) -> PlaybackSearchFailure {
    match error {
        PlayerSearchClientError::QueueFull => PlaybackSearchFailure::Busy,
        PlayerSearchClientError::RuntimeStopped => {
            PlaybackSearchFailure::Unavailable("runtime stopped".to_string())
        }
        PlayerSearchClientError::OperationIdExhausted => {
            PlaybackSearchFailure::Unavailable("operation id exhausted".to_string())
        }
        PlayerSearchClientError::NotRun { reason } => PlaybackSearchFailure::Unavailable(reason),
        PlayerSearchClientError::Failed(error) => PlaybackSearchFailure::Backend(error.to_string()),
        PlayerSearchClientError::UnexpectedOutcome(outcome) => {
            PlaybackSearchFailure::Unexpected(outcome.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{automatic_advance_allowed, immediate_playback_evidence};
    use crate::config::LiveConfigs;
    use crate::features::playback::PlaybackCommand;

    #[test]
    fn partial_playback_error_is_distinguished_from_completed_command() {
        let command = PlaybackCommand::Pause;
        let error: anyhow::Result<()> = Err(anyhow::anyhow!("reply failed after pause"));
        let (status, result) = immediate_playback_evidence(&command, &error);
        assert_eq!(status, "failed");
        assert_eq!(result, "handler_error; effects_may_be_partial: pause");

        let success: anyhow::Result<()> = Ok(());
        let (status, result) = immediate_playback_evidence(&command, &success);
        assert_eq!(status, "completed");
        assert_eq!(result, "pause");
    }

    #[test]
    fn pending_reload_suppresses_automatic_queue_advance() {
        let live = LiveConfigs::default();
        assert!(automatic_advance_allowed(&live));

        live.schedule_reload(["queue.max_size".to_string()]);
        assert!(!automatic_advance_allowed(&live));
    }
}
