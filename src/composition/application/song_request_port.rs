use super::*;

use crate::features::playback::{PlaybackResult, QueuePushOutcome};
use crate::features::song_request::{PickedCandidate, SearchCandidate};
use crate::features::song_request::{
    SongRequestContext, SongRequestDecision, SongRequestPort, SongSearchFailure,
};

impl SongRequestPort for ApplicationRuntime {
    fn song_ai_command_allowed(
        &self,
        username: &str,
        account: Option<&crate::features::command::CommandAccount>,
        authority: crate::features::command::CommandAuthority,
        console: bool,
    ) -> bool {
        if console {
            return true;
        }
        let snapshot = self.lifecycle.live_configs.snapshot();
        // 与路由期**完全同一口径**：有账号按 UID，否则按昵称；
        // 身份命中就覆盖，未命中退回来源下界（粉色好友→Friend，蓝色大厅→User）。
        // 少了 floor 就会出现「路由放行、执行拒绝」，用户看到的是权限不够。
        let actor = crate::features::command_access::actor_permission_with_floor(
            self.lifecycle
                .live_configs
                .identity
                .role_of_sender(username, account),
            authority,
        );
        crate::features::command_access::catalog_policy(&snapshot.command_access, "@AI点歌")
            .is_some_and(|policy| policy.allows(Some(actor)))
    }

    fn song_source_allowed(
        &self,
        username: &str,
        account: Option<&crate::features::command::CommandAccount>,
        authority: crate::features::command::CommandAuthority,
        source: &str,
        console: bool,
    ) -> bool {
        if console {
            return true;
        }
        let Some(source) = crate::features::song_request::SongSource::from_str(source) else {
            return false;
        };
        let Some(id) = source.permission_id() else {
            return false;
        };
        let snapshot = self.lifecycle.live_configs.snapshot();
        let actor = crate::features::command_access::actor_permission_with_floor(
            self.lifecycle
                .live_configs
                .identity
                .role_of_sender(username, account),
            authority,
        );
        crate::features::command_access::catalog_policy(&snapshot.command_access, id)
            .is_some_and(|policy| policy.allows(Some(actor)))
    }

    fn reply(&self, message: &str) -> Result<()> {
        ApplicationRuntime::reply(self, message)
    }

    fn reply_template(
        &self,
        spec: &crate::features::reply_template::ReplyTemplate,
        vars: &crate::features::reply_template::ReplyVars,
    ) -> Result<()> {
        self.reply_with_template(spec, vars)
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
        // 本地推荐策略在 b8605a5 从 ai 段迁到了 song_ai 段（点歌专属）。
        self.lifecycle
            .live_configs
            .snapshot()
            .song_ai
            .local_recommend
    }

    fn local_recommend_min_score(&self) -> f64 {
        self.lifecycle
            .live_configs
            .snapshot()
            .song_ai
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
                log::warn!(
                    "读取 AI 点歌的曲库播放池失败（步骤：汇总本地曲库候选），本次本地推荐少掉播放池这一路，只剩引擎缓存里的记录；本地推荐命中率下降，AI 点歌会更多退回平台搜索: {error}"
                );
                Vec::new()
            }
        };
        match self.playback.native_playback.search_library(query.clone()) {
            Ok(candidates) => tracks.extend(
                candidates
                    .into_iter()
                    .map(|candidate| candidate.playable_track()),
            ),
            Err(error) => log::warn!(
                "读取 AI 点歌的引擎曲库缓存失败（步骤：汇总本地曲库候选），本次本地推荐只剩播放池这一路，没有缓存记录可用；本地推荐命中率下降，AI 点歌会更多退回平台搜索: {error}"
            ),
        }
        Ok(miliastra_playback::search_library_tracks(&query, tracks))
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
        PlayerSearchClientError::Failed(error) => match error.code() {
            Some("provider_auth_required") => SongSearchFailure::AuthRequired,
            Some("relogin_required") => SongSearchFailure::ReloginRequired,
            Some("provider_rate_limited") => SongSearchFailure::RateLimited,
            _ => SongSearchFailure::Backend(error.to_string()),
        },
        PlayerSearchClientError::UnexpectedOutcome(outcome) => {
            SongSearchFailure::Unexpected(outcome.to_string())
        }
    }
}
