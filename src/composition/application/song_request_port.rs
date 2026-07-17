use super::*;

use crate::features::playback::{PlaybackOutcome, QueuePushOutcome};
use crate::features::song_request::{PickedCandidate, SearchCandidate};
use crate::features::song_request::{
    SongRequestContext, SongRequestDecision, SongRequestPort, SongSearchFailure,
};

impl SongRequestPort for ApplicationRuntime {
    fn reply(&self, message: &str) -> Result<()> {
        ApplicationRuntime::reply(self, message)
    }

    fn wait_for_decision(
        &mut self,
        allow_switch_source: bool,
        allow_ai: bool,
        default_confirm: bool,
    ) -> Result<SongRequestDecision> {
        ApplicationRuntime::wait_for_decision(self, allow_switch_source, allow_ai, default_confirm)
    }

    fn search_candidates(
        &self,
        keyword: &str,
        source: &str,
    ) -> std::result::Result<Option<Vec<SearchCandidate>>, SongSearchFailure> {
        self.player_search
            .search_candidates(keyword, source)
            .map(|candidates| (!candidates.is_empty()).then_some(candidates))
            .map_err(song_search_failure)
    }

    fn search_and_pick(
        &self,
        keyword: &str,
        source: &str,
        prefer_accompaniment: bool,
    ) -> std::result::Result<Option<PickedCandidate>, SongSearchFailure> {
        self.player_search
            .search_and_pick(keyword, source, prefer_accompaniment)
            .map_err(song_search_failure)
    }

    fn playback_queue(&self) -> Result<Vec<QueueItem>> {
        ApplicationRuntime::playback_queue(self)
    }

    fn queue_contains(&self, item: QueueItem) -> Result<bool> {
        self.business
            .playback_queue_contains(item)
            .map_err(anyhow::Error::from)
    }

    fn push_queue(&self, item: QueueItem) -> Result<QueuePushOutcome> {
        self.business
            .push_playback_queue(item)
            .map_err(anyhow::Error::from)
    }

    fn player_status(&self) -> Result<PlayerStatus> {
        self.player.status()
    }

    fn should_queue_until_current_song_finished(&self, status: &PlayerStatus) -> Result<bool> {
        self.player.should_queue_until_current_song_finished(status)
    }

    fn current_status_matches_request(&self, status: &PlayerStatus) -> Result<bool> {
        self.player.current_status_matches_request(status)
    }

    fn play_confirmed(
        &mut self,
        request: &ResolvedSongRequest,
        allow_switch_source: bool,
    ) -> Result<PlaybackOutcome> {
        self.play_request_confirmed(request, allow_switch_source)
    }

    fn song_dedup_limited(&self, request: &PlaybackRequest) -> Result<bool> {
        self.player.song_dedup_limited(request)
    }

    fn log_executed(&self, context: &SongRequestContext, final_command: &str) -> Result<()> {
        self.log_executed_command_fields(
            &context.message_type,
            &context.username,
            &context.user_command,
            final_command,
        )
    }
}

fn song_search_failure(error: PlayerSearchClientError) -> SongSearchFailure {
    match error {
        PlayerSearchClientError::QueueFull => SongSearchFailure::Busy,
        PlayerSearchClientError::RuntimeStopped => {
            SongSearchFailure::Unavailable("runtime stopped".to_string())
        }
        PlayerSearchClientError::OperationIdExhausted => {
            SongSearchFailure::Unavailable("operation id exhausted".to_string())
        }
        PlayerSearchClientError::NotRun { reason } => SongSearchFailure::Unavailable(reason),
        PlayerSearchClientError::Failed(error) => SongSearchFailure::Backend(error.to_string()),
        PlayerSearchClientError::UnexpectedOutcome(outcome) => {
            SongSearchFailure::Unexpected(outcome.to_string())
        }
    }
}
