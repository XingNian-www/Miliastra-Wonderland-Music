use super::*;

use crate::features::playback::{
    PlaybackAttempt, PlaybackCommandContext, PlaybackCommandPort, PlaybackDecision,
    PlaybackExecutionPort, PlaybackPickedCandidate, PlaybackSearchFailure, PlaybackSelection,
    PlaybackVerification, QueueRemoval,
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
        self.playback_application
            .clone()
            .execute_command(&context, command, self)
    }

    pub(super) fn execute_advance_queue_task(&mut self, reason: &'static str) -> Result<()> {
        self.playback_application
            .clone()
            .consume_queue(reason, self)
    }

    pub(super) fn play_request_confirmed(
        &mut self,
        request: &ResolvedSongRequest,
        allow_switch_source: bool,
    ) -> Result<crate::features::playback::PlaybackOutcome> {
        let selection = PlaybackSelection {
            keyword: request.keyword.clone(),
            source: request.source.clone(),
            prefer_accompaniment: request.prefer_accompaniment,
            ai_original_text: request.ai_original_text.clone(),
            uri: request.uri.clone(),
            friend_username: request.friend_username.clone(),
            console_bypass_dedup: request.console_bypass_dedup,
        };
        self.playback_application
            .clone()
            .play_confirmed(&selection, allow_switch_source, self)
    }

    pub(super) fn wait_for_decision(
        &mut self,
        allow_switch_source: bool,
        allow_ai: bool,
        timeout_confirms: bool,
    ) -> Result<SongRequestDecision> {
        let accepts_message_type = |message_type: &str| message_type == "blue";
        let is_decision = |text: &str| SongRequestDecision::parse(text).is_some();
        let mut reader = self.begin_chat_decision_reader(
            ChatDecisionScope::CurrentHall,
            &accepts_message_type,
            &is_decision,
        )?;
        let timeout = Duration::from_millis(self.config.timing.decision.timeout_ms);
        let map_web_decision = |decision| match decision {
            DecisionAction::Confirm => SongRequestDecision::Confirm,
            DecisionAction::Skip => SongRequestDecision::Skip,
            DecisionAction::SwitchSource => SongRequestDecision::SwitchSource,
            DecisionAction::Ai => SongRequestDecision::Ai,
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
        if !self.running.load(AtomicOrdering::SeqCst) {
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
        self.player_search
            .search_and_pick(keyword, source, prefer_accompaniment)
            .map(|picked| {
                picked.map(|picked| PlaybackPickedCandidate {
                    text: picked.candidate.text,
                    uri: picked.candidate.uri,
                })
            })
            .map_err(playback_search_failure)
    }

    fn song_dedup_limited(&mut self, request: &PlaybackRequest) -> Result<bool> {
        self.player.song_dedup_limited(request)
    }

    fn play_request_uri(&mut self, request: &PlaybackRequest) -> Result<PlaybackAttempt> {
        self.player.play_request_uri(request)
    }

    fn verify_playback_started(
        &mut self,
        request: &PlaybackRequest,
        attempt: &mut PlaybackAttempt,
    ) -> Result<PlaybackVerification> {
        self.player.verify_playback_started(request, attempt)
    }

    fn reject_mismatch_as_no_source(&mut self, status: Option<&PlayerStatus>) -> Result<()> {
        self.player.reject_mismatch_as_no_source(status)
    }

    fn player_status(&mut self) -> Result<PlayerStatus> {
        self.player.status()
    }

    fn wait_for_decision(
        &mut self,
        allow_switch_source: bool,
        allow_ai: bool,
        timeout_confirms: bool,
    ) -> Result<PlaybackDecision> {
        Ok(
            match ApplicationRuntime::wait_for_decision(
                self,
                allow_switch_source,
                allow_ai,
                timeout_confirms,
            )? {
                SongRequestDecision::Confirm => PlaybackDecision::Confirm,
                SongRequestDecision::Skip => PlaybackDecision::Skip,
                SongRequestDecision::SwitchSource => PlaybackDecision::SwitchSource,
                SongRequestDecision::Ai => PlaybackDecision::Ai,
                SongRequestDecision::Timeout => PlaybackDecision::Timeout,
                SongRequestDecision::Stopped => PlaybackDecision::Stopped,
            },
        )
    }

    fn playback_queue(&mut self) -> Result<Vec<QueueItem>> {
        ApplicationRuntime::playback_queue(self)
    }

    fn remove_playback_queue(&mut self, removal: QueueRemoval) -> Result<()> {
        self.business
            .remove_playback_queue(removal)
            .map(|_| ())
            .map_err(anyhow::Error::from)
    }
}

impl PlaybackCommandPort for ApplicationRuntime {
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
        self.player.pause_by_user()
    }

    fn resume_by_user(&mut self) -> Result<String> {
        self.player.resume_by_user()
    }

    fn next_external(&mut self) -> Result<String> {
        self.player.next_external()
    }

    fn previous_external(&mut self) -> Result<String> {
        self.player.previous_external()
    }

    fn set_volume(&mut self, volume: &str) -> Result<()> {
        self.player.set_volume(volume).map(|_| ())
    }

    fn remove_playback_queue_indexes(
        &mut self,
        indexes: Vec<usize>,
    ) -> Result<Vec<(usize, QueueItem)>> {
        self.business
            .remove_playback_queue_indexes(indexes)
            .map_err(anyhow::Error::from)
    }

    fn clear_playback_queue(&mut self) -> Result<usize> {
        self.business
            .clear_playback_queue()
            .map_err(anyhow::Error::from)
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
