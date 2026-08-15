use super::*;

use std::collections::HashSet;

use miliastra_playback::PlayableTrack;

use crate::features::playback::{
    BackgroundLyricsScope, PlaybackCommandContext, PlaybackCommandPort, PlaybackExecutionPort,
    PlaybackPickedCandidate, PlaybackResult, PlaybackSearchFailure, PlaybackVerification,
    QueueRemoval,
};

impl ApplicationRuntime {
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
        self.playback
            .playback_application
            .clone()
            .execute_command(&context, command, self)
    }

    pub(super) fn execute_advance_queue_task(&mut self, reason: &'static str) -> Result<()> {
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

    pub(super) fn prompt_and_wait_for_decision(
        &mut self,
        message: &str,
        allow_switch_source: bool,
        allow_ai: bool,
        timeout_confirms: bool,
    ) -> Result<SongRequestDecision> {
        let reader = self.begin_song_decision_reader()?;
        self.reply(message)?;
        self.wait_for_decision_with_reader(reader, allow_switch_source, allow_ai, timeout_confirms)
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
            let wait = Duration::from_millis(
                reader.poll_interval_ms(self.lifecycle.config.timing.decision.poll_ms),
            )
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
                    || SongRequestDecision::is_feedback_text(&message.text)
                {
                    continue;
                }
                let Some(decision) = SongRequestDecision::parse(&message.text) else {
                    continue;
                };
                if !reader.accept_once(&message) {
                    continue;
                }
                match decision {
                    SongRequestDecision::Confirm => return Ok(SongRequestDecision::Confirm),
                    SongRequestDecision::Skip => return Ok(SongRequestDecision::Skip),
                    SongRequestDecision::SwitchSource if allow_switch_source => {
                        return Ok(SongRequestDecision::SwitchSource);
                    }
                    SongRequestDecision::Ai if allow_ai => return Ok(SongRequestDecision::Ai),
                    _ => {}
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
            ApplicationRuntime::reply(
                self,
                if allow_switch_source {
                    "此平台匹配失败,命令已超时(20s)下次可以尝试@确认@跳过@换源"
                } else {
                    "此平台匹配失败,命令已超时(20s)下次可以尝试@确认@跳过"
                },
            )?;
            Ok(SongRequestDecision::Timeout)
        }
    }
}

impl PlaybackExecutionPort for ApplicationRuntime {
    fn reply(&mut self, message: &str) -> Result<()> {
        ApplicationRuntime::reply(self, message)
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

    fn ai_search_and_pick(
        &mut self,
        keyword: &str,
        source: &str,
        prefer_accompaniment: bool,
    ) -> std::result::Result<Option<PlaybackPickedCandidate>, PlaybackSearchFailure> {
        if !self.business.ai.enabled() {
            return Err(PlaybackSearchFailure::Unavailable(
                "点歌 AI 未启用".to_string(),
            ));
        }
        let candidates = self
            .playback
            .player_search
            .search_candidates(keyword, source)
            .map_err(playback_search_failure)?;
        if candidates.is_empty() {
            return Ok(None);
        }
        let (candidate, _pick) = select_ai_candidate(
            &self.business.ai,
            keyword,
            prefer_accompaniment,
            &candidates,
        )
        .map_err(|error| PlaybackSearchFailure::Backend(error.to_string()))?;
        Ok(Some(PlaybackPickedCandidate {
            track: candidate.playable_track(),
            text: candidate.text,
            candidate_snapshot: candidates,
        }))
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

    fn player_status(&mut self) -> Result<PlayerStatus> {
        self.playback.player.status()
    }

    fn playback_queue(&mut self) -> Result<Vec<QueueItem>> {
        ApplicationRuntime::playback_queue(self)
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
        self.business.background_commands.stop("lyrics")
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
