use super::*;

use crate::features::playback::{PlaybackResult, QueuePushOutcome};
use crate::features::song_request::{PickedCandidate, SearchCandidate};
use crate::features::song_request::{
    SongRequestContext, SongRequestDecision, SongRequestPort, SongSearchFailure,
};

impl SongRequestPort for ApplicationRuntime {
    fn reply(&self, message: &str) -> Result<()> {
        ApplicationRuntime::reply(self, message)
    }

    fn prompt_and_wait_for_decision_batch(
        &mut self,
        messages: &[String],
        allow_switch_source: bool,
        allow_ai: bool,
        allow_local: bool,
        default_confirm: bool,
    ) -> Result<SongRequestDecision> {
        ApplicationRuntime::prompt_and_wait_for_decision_batch(
            self,
            messages,
            allow_switch_source,
            allow_ai,
            allow_local,
            default_confirm,
        )
    }

    fn search_candidates(
        &self,
        keyword: &str,
        source: &str,
    ) -> std::result::Result<Option<Vec<SearchCandidate>>, SongSearchFailure> {
        self.playback
            .player_search
            .search_candidates(keyword, source)
            .map(|candidates| (!candidates.is_empty()).then_some(candidates))
            .map_err(song_search_failure)
    }

    fn local_recommend(&self) -> bool {
        self.lifecycle.live_configs.snapshot().ai.local_recommend
    }

    fn local_recommend_min_score(&self) -> f64 {
        self.lifecycle
            .live_configs
            .snapshot()
            .ai
            .local_recommend_min_score
    }

    fn search_library_candidates(
        &self,
        keyword: &str,
        allow_bilibili: bool,
    ) -> Result<Vec<SearchCandidate>> {
        // 本地兜底跨平台：在线检索仍按请求的平台，本地曲库不再跟着某个平台走，
        // 只在缺少好友及以上权限时排除 B站 音源。
        let providers = if allow_bilibili {
            Vec::new()
        } else {
            miliastra_playback::ProviderId::ALL
                .into_iter()
                .filter(|provider| *provider != miliastra_playback::ProviderId::Bilibili)
                .collect()
        };
        let query = miliastra_playback::SearchQuery {
            keyword: keyword.to_owned(),
            providers,
            limit: miliastra_playback::MAX_LIBRARY_SEARCH_RESULTS,
        };
        // 播放池保留完整 resolver locator，优先于仅有元数据的缓存记录。
        let mut tracks = match self.business.business.playback_pool_snapshot() {
            Ok(tracks) => tracks,
            Err(error) => {
                log::warn!("读取AI点歌曲库播放池失败: {error}");
                Vec::new()
            }
        };
        match self.playback.native_playback.search_library(query.clone()) {
            Ok(candidates) => tracks.extend(
                candidates
                    .into_iter()
                    .map(|candidate| candidate.playable_track()),
            ),
            Err(error) => log::warn!("读取AI点歌曲库缓存失败: {error}"),
        }
        Ok(miliastra_playback::search_library_tracks(&query, tracks))
    }

    fn online_candidate_matches_keyword(&self, keyword: &str, candidate: &SearchCandidate) -> bool {
        let query = miliastra_playback::SearchQuery {
            keyword: keyword.to_owned(),
            providers: vec![candidate.track_ref.key.provider],
            limit: 1,
        };
        !miliastra_playback::search_library_tracks(&query, [candidate.playable_track()]).is_empty()
    }

    fn search_and_pick(
        &self,
        keyword: &str,
        source: &str,
        prefer_accompaniment: bool,
    ) -> std::result::Result<Option<PickedCandidate>, SongSearchFailure> {
        self.playback
            .player_search
            .search_and_pick(keyword, source, prefer_accompaniment)
            .map_err(song_search_failure)
    }

    fn playback_queue(&self) -> Result<Vec<QueueItem>> {
        ApplicationRuntime::playback_queue(self)
    }

    fn queue_contains(&self, item: QueueItem) -> Result<bool> {
        self.business
            .business
            .playback_queue_contains(item)
            .map_err(anyhow::Error::from)
    }

    fn push_queue(&self, item: QueueItem) -> Result<QueuePushOutcome> {
        self.business
            .business
            .push_playback_queue(item)
            .map_err(anyhow::Error::from)
    }

    fn preload_track(&self, track: &PlayableTrack) -> Result<()> {
        self.playback
            .native_playback
            .preload(track.clone())
            .map_err(anyhow::Error::from)
    }

    fn player_status(&self) -> Result<PlayerStatus> {
        self.playback.player.status()
    }

    fn should_queue_until_current_song_finished(&self, status: &PlayerStatus) -> Result<bool> {
        self.playback
            .player
            .should_queue_until_current_song_finished(status)
    }

    fn current_status_matches_request(&self, status: &PlayerStatus) -> Result<bool> {
        self.playback.player.current_status_matches_request(status)
    }

    fn play_confirmed(&mut self, request: &ResolvedSongRequest) -> Result<PlaybackResult> {
        self.play_request_confirmed(request)
    }

    fn song_dedup_limited(&self, request: &PlaybackRequest) -> Result<bool> {
        self.playback.player.song_dedup_limited(request)
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
