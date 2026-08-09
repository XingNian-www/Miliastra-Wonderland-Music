use std::fmt::{Display, Formatter};
use std::sync::Arc;

use anyhow::{Result, anyhow};
use miliastra_playback::PlayableTrack;

use super::{
    AiCandidatePickResult, AiClient, SongCommand, SongReviewCandidate, SongReviewClient,
    SongReviewDecision, SongSource, split_candidate_title_artist,
};
use super::{PickedCandidate, SearchCandidate};
use crate::features::playback::{
    PlaybackOutcome, PlaybackRequest, PlaybackResult, PlaybackSelection, PlayerStatus, QueueItem,
    QueuePushOutcome, is_playing,
};

#[derive(Clone, Debug)]
pub(crate) struct SongRequestContext {
    pub(crate) message_type: String,
    pub(crate) raw: String,
    pub(crate) username: String,
    pub(crate) user_command: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SongRequestDecision {
    Confirm,
    Skip,
    SwitchSource,
    Ai,
    Timeout,
    Stopped,
}

impl SongRequestDecision {
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let raw = text.trim();
        let command_text = if let Some(index) = raw.find(['：', ':', ']', '】']) {
            let separator_len = raw[index..].chars().next()?.len_utf8();
            &raw[index + separator_len..]
        } else {
            raw
        }
        .trim_start_matches(['：', ':', ' ', '\t', ']', '】']);
        if command_text
            .get(..3)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("@AI"))
            && decision_boundary(command_text[3..].chars().next())
        {
            return Some(Self::Ai);
        }
        if command_text
            .strip_prefix("@确认")
            .is_some_and(|rest| decision_boundary(rest.chars().next()))
        {
            Some(Self::Confirm)
        } else if command_text
            .strip_prefix("@跳过")
            .is_some_and(|rest| decision_boundary(rest.chars().next()))
        {
            Some(Self::Skip)
        } else if command_text
            .strip_prefix("@换源")
            .is_some_and(|rest| decision_boundary(rest.chars().next()))
        {
            Some(Self::SwitchSource)
        } else {
            None
        }
    }

    pub(crate) fn is_feedback_text(text: &str) -> bool {
        [
            "匹配失败",
            "AI自动匹配",
            "换源结果",
            "换源到",
            "换源后仍无音源",
            "下次可以尝试",
            "如非预期",
            "命令已超时",
            "搜索到:",
            "AI匹配:",
            "AI匹配中",
            "AI点歌未启用",
            "AI点歌识别失败",
        ]
        .iter()
        .any(|pattern| text.contains(pattern))
    }
}

fn decision_boundary(ch: Option<char>) -> bool {
    match ch {
        None => true,
        Some(ch) => {
            ch.is_whitespace()
                || matches!(
                    ch,
                    '，' | ',' | '。' | '.' | '!' | '！' | '?' | '？' | ']' | '】'
                )
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SongSearchFailure {
    Busy,
    Unavailable(String),
    Backend(String),
    Unexpected(String),
}

impl SongSearchFailure {
    fn user_message(&self) -> &'static str {
        match self {
            Self::Busy => "歌曲搜索繁忙，请稍后再试",
            Self::Unavailable(_) => "歌曲搜索服务暂不可用，请稍后再试",
            Self::Backend(_) => "歌曲搜索后端失败，请稍后再试",
            Self::Unexpected(_) => "歌曲搜索后端返回异常，请稍后再试",
        }
    }
}

impl Display for SongSearchFailure {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => formatter.write_str("player search queue full"),
            Self::Unavailable(reason) => write!(formatter, "player search unavailable: {reason}"),
            Self::Backend(reason) => write!(formatter, "player search backend failed: {reason}"),
            Self::Unexpected(reason) => {
                write!(formatter, "unexpected player search outcome: {reason}")
            }
        }
    }
}

pub(crate) trait SongRequestPort {
    fn reply(&self, message: &str) -> Result<()>;

    fn prompt_and_wait_for_decision(
        &mut self,
        message: &str,
        allow_switch_source: bool,
        allow_ai: bool,
        default_confirm: bool,
    ) -> Result<SongRequestDecision>;

    fn search_candidates(
        &self,
        keyword: &str,
        source: &str,
    ) -> std::result::Result<Option<Vec<SearchCandidate>>, SongSearchFailure>;

    fn search_and_pick(
        &self,
        keyword: &str,
        source: &str,
        prefer_accompaniment: bool,
    ) -> std::result::Result<Option<PickedCandidate>, SongSearchFailure>;

    fn playback_queue(&self) -> Result<Vec<QueueItem>>;
    fn queue_contains(&self, item: QueueItem) -> Result<bool>;
    fn push_queue(&self, item: QueueItem) -> Result<QueuePushOutcome>;
    fn player_status(&self) -> Result<PlayerStatus>;
    fn should_queue_until_current_song_finished(&self, status: &PlayerStatus) -> Result<bool>;
    fn current_status_matches_request(&self, status: &PlayerStatus) -> Result<bool>;
    fn play_confirmed(&mut self, request: &ResolvedSongRequest) -> Result<PlaybackResult>;
    fn song_dedup_limited(&self, request: &PlaybackRequest) -> Result<bool>;
    fn log_executed(&self, context: &SongRequestContext, final_command: &str) -> Result<()>;
}

pub(crate) trait SongRequestAiGateway: Send + Sync {
    fn enabled(&self) -> bool;
    fn pick_song_candidate(
        &self,
        request: &str,
        prefer_accompaniment: bool,
        candidates: &[SearchCandidate],
    ) -> Result<AiCandidatePickResult>;
}

pub(crate) fn select_ai_candidate(
    ai: &dyn SongRequestAiGateway,
    request: &str,
    prefer_accompaniment: bool,
    candidates: &[SearchCandidate],
) -> Result<(SearchCandidate, AiCandidatePickResult)> {
    let pick = ai.pick_song_candidate(request, prefer_accompaniment, candidates)?;
    let candidate = pick
        .index
        .checked_sub(1)
        .and_then(|index| candidates.get(index))
        .cloned()
        .ok_or_else(|| anyhow!("AI 返回未知歌曲候选索引: {}", pick.index))?;
    Ok((candidate, pick))
}

impl SongRequestAiGateway for AiClient {
    fn enabled(&self) -> bool {
        AiClient::enabled(self)
    }

    fn pick_song_candidate(
        &self,
        request: &str,
        prefer_accompaniment: bool,
        candidates: &[SearchCandidate],
    ) -> Result<AiCandidatePickResult> {
        AiClient::pick_song_candidate(self, request, prefer_accompaniment, candidates)
    }
}

pub(crate) trait SongReviewGateway: Send + Sync {
    fn enabled(&self) -> bool;
    fn reply_reason_max_chars(&self) -> usize;
    fn review(&self, candidate: &SongReviewCandidate) -> SongReviewDecision;
}

impl SongReviewGateway for SongReviewClient {
    fn enabled(&self) -> bool {
        SongReviewClient::enabled(self)
    }

    fn reply_reason_max_chars(&self) -> usize {
        SongReviewClient::reply_reason_max_chars(self)
    }

    fn review(&self, candidate: &SongReviewCandidate) -> SongReviewDecision {
        SongReviewClient::review(self, candidate)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedSongRequest {
    pub(crate) keyword: String,
    pub(crate) source: String,
    pub(crate) prefer_accompaniment: bool,
    pub(crate) ai_original_text: String,
    pub(crate) track: Option<PlayableTrack>,
    pub(crate) friend_username: String,
    pub(crate) requester: String,
    pub(crate) console_bypass_dedup: bool,
    pub(crate) candidate_snapshot: Vec<SearchCandidate>,
}

impl ResolvedSongRequest {
    pub(crate) fn label(&self) -> String {
        source_label(&self.friend_username)
    }

    pub(crate) fn playback_request(&self) -> PlaybackRequest {
        self.playback_selection().request()
    }

    pub(crate) fn playback_selection(&self) -> PlaybackSelection {
        PlaybackSelection {
            keyword: self.keyword.clone(),
            source: self.source.clone(),
            prefer_accompaniment: self.prefer_accompaniment,
            ai_original_text: self.ai_original_text.clone(),
            track: self.track.clone(),
            friend_username: self.friend_username.clone(),
            requester: self.requester.clone(),
            console_bypass_dedup: self.console_bypass_dedup,
            candidate_snapshot: self.candidate_snapshot.clone(),
        }
    }

    pub(crate) fn dedup_reject_message(&self) -> String {
        format!("{}近期已播放过,请稍后再点", self.keyword)
    }

    fn final_command(&self, action: &str) -> String {
        self.final_command_for_playback(action, &self.playback_request())
    }

    fn final_command_for_playback(
        &self,
        action: &str,
        playback_request: &PlaybackRequest,
    ) -> String {
        let source = if playback_request.source.trim().is_empty() {
            "all"
        } else {
            playback_request.source.trim()
        };
        format!(
            "{} keyword={} source={} uri={} aiOriginal={}",
            action,
            playback_request.keyword,
            source,
            playback_request.uri(),
            self.ai_original_text,
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SongQueuePushOutcome {
    Added(usize),
    Full,
    DedupLimited,
}

#[derive(Clone, Copy)]
struct QueuePushFeedback {
    queued_action: &'static str,
    full_action: &'static str,
    queued_prefix: &'static str,
    full_reply: &'static str,
}

const QUEUE_PUSH_FEEDBACK: QueuePushFeedback = QueuePushFeedback {
    queued_action: "queue",
    full_action: "queue-full",
    queued_prefix: "队列已加入",
    full_reply: "队列已满，请稍后再试",
};

const UNKNOWN_STATUS_QUEUE_PUSH_FEEDBACK: QueuePushFeedback = QueuePushFeedback {
    queued_action: "queue-status-unknown",
    full_action: "queue-full-status-unknown",
    queued_prefix: "状态未知，队列已加入",
    full_reply: "状态未知且队列已满，请稍后再试",
};

#[derive(Clone)]
pub(crate) struct SongRequestApplication {
    ai: Arc<dyn SongRequestAiGateway>,
    song_review: Arc<dyn SongReviewGateway>,
    queue_max_size: usize,
    console_bypass_dedup: bool,
}

impl SongRequestApplication {
    pub(crate) fn new(
        ai: AiClient,
        song_review: SongReviewClient,
        queue_max_size: usize,
        console_bypass_dedup: bool,
    ) -> Self {
        Self {
            ai: Arc::new(ai),
            song_review: Arc::new(song_review),
            queue_max_size,
            console_bypass_dedup,
        }
    }

    #[cfg(test)]
    fn with_gateways(
        ai: Arc<dyn SongRequestAiGateway>,
        song_review: Arc<dyn SongReviewGateway>,
        queue_max_size: usize,
        console_bypass_dedup: bool,
    ) -> Self {
        Self {
            ai,
            song_review,
            queue_max_size,
            console_bypass_dedup,
        }
    }

    pub(crate) fn execute(
        &self,
        context: &SongRequestContext,
        song: &SongCommand,
        port: &mut dyn SongRequestPort,
    ) -> Result<()> {
        SongRequestExecution {
            ai: self.ai.as_ref(),
            song_review: self.song_review.as_ref(),
            queue_max_size: self.queue_max_size,
            console_bypass_dedup: self.console_bypass_dedup,
            port,
        }
        .execute_song_request_intent(context, song)
    }
}

struct SongRequestExecution<'a> {
    ai: &'a dyn SongRequestAiGateway,
    song_review: &'a dyn SongReviewGateway,
    queue_max_size: usize,
    console_bypass_dedup: bool,
    port: &'a mut dyn SongRequestPort,
}

impl SongRequestExecution<'_> {
    fn execute_song_request_intent(
        &mut self,
        context: &SongRequestContext,
        song: &SongCommand,
    ) -> Result<()> {
        let Some(mut request) = self.resolve_and_confirm_song(song)? else {
            return Ok(());
        };
        request.requester = context.username.clone();
        request.console_bypass_dedup = context.message_type == "控制台";
        if !self.review_song_candidate(context, &request)? {
            return Ok(());
        }
        if self.queue_contains_request(&request)? {
            log::info!("队列已有: {}", request.keyword);
            self.log_executed_command(context, &request.final_command("duplicate"))?;
            self.reply(&format!("队列已有: {}", request.keyword))?;
            return Ok(());
        }
        if !self.playback_queue()?.is_empty() {
            let outcome = self.push_queue_request(&request)?;
            self.handle_queue_push_outcome(context, &request, outcome, QUEUE_PUSH_FEEDBACK)?;
            return Ok(());
        }

        let status = self.port.player_status();
        match status {
            Ok(status) if is_playing(&status) => {
                if request.track.as_ref().is_some_and(|requested| {
                    status
                        .current_track
                        .as_ref()
                        .is_some_and(|current| current.track_ref.key == requested.track_ref.key)
                }) {
                    self.log_executed_command(context, &request.final_command("already-playing"))?;
                    self.reply(&format!("当前正在播放: {}", request.keyword))?;
                    return Ok(());
                }
                if self
                    .port
                    .should_queue_until_current_song_finished(&status)?
                {
                    let outcome = self.push_queue_request(&request)?;
                    self.handle_queue_push_outcome(
                        context,
                        &request,
                        outcome,
                        QUEUE_PUSH_FEEDBACK,
                    )?;
                    return Ok(());
                }
                if !self.port.current_status_matches_request(&status)? {
                    let result = self.play_request_confirmed(&request)?;
                    self.log_play_request_outcome(context, &request, &result)?;
                    return Ok(());
                }
                let outcome = self.push_queue_request(&request)?;
                self.handle_queue_push_outcome(context, &request, outcome, QUEUE_PUSH_FEEDBACK)?;
                return Ok(());
            }
            Ok(status) => {
                if self
                    .port
                    .should_queue_until_current_song_finished(&status)?
                {
                    let outcome = self.push_queue_request(&request)?;
                    self.handle_queue_push_outcome(
                        context,
                        &request,
                        outcome,
                        QUEUE_PUSH_FEEDBACK,
                    )?;
                    return Ok(());
                }
            }
            Err(error) => {
                log::error!("获取播放状态失败: {error:#}");
                let outcome = self.push_queue_request(&request)?;
                self.handle_queue_push_outcome(
                    context,
                    &request,
                    outcome,
                    UNKNOWN_STATUS_QUEUE_PUSH_FEEDBACK,
                )?;
                return Ok(());
            }
        }

        let result = self.play_request_confirmed(&request)?;
        self.log_play_request_outcome(context, &request, &result)
    }

    fn report_player_search_failure(
        &self,
        label: &str,
        context: &str,
        error: &SongSearchFailure,
    ) -> Result<()> {
        log::error!("{context}: {error}");
        self.reply(&format!("{}{}", label, error.user_message()))
    }

    fn resolve_song_request(&mut self, song: &SongCommand) -> Result<Option<ResolvedSongRequest>> {
        if !song.ai_assisted {
            return Ok(Some(ResolvedSongRequest {
                keyword: song.keyword.clone(),
                source: song.source.as_str().to_string(),
                prefer_accompaniment: song.prefer_accompaniment,
                ai_original_text: String::new(),
                track: None,
                friend_username: song.friend_username.clone(),
                requester: String::new(),
                console_bypass_dedup: false,
                candidate_snapshot: Vec::new(),
            }));
        }
        self.resolve_ai_song_request(song, true)
    }

    fn resolve_ai_song_request(
        &mut self,
        song: &SongCommand,
        include_configuration_hint: bool,
    ) -> Result<Option<ResolvedSongRequest>> {
        let label = song_label(song);
        if !self.ai.enabled() {
            self.reply(&format!(
                "{}{}",
                label,
                if include_configuration_hint {
                    "AI点歌未启用，请先配置 ai.api_key"
                } else {
                    "AI点歌未启用"
                }
            ))?;
            return Ok(None);
        }

        self.reply(&format!("{}AI匹配中", label))?;

        let search_source = ai_candidate_source(song);
        let candidates = match self.port.search_candidates(&song.keyword, search_source) {
            Ok(Some(candidates)) => candidates,
            Ok(None) => {
                self.reply(&format!("{}平台无对应歌曲音源", label))?;
                return Ok(None);
            }
            Err(error) => {
                self.report_player_search_failure(&label, "AI点歌搜索候选失败", &error)?;
                return Ok(None);
            }
        };

        let (candidate, pick) = match select_ai_candidate(
            self.ai,
            &song.keyword,
            song.prefer_accompaniment,
            &candidates,
        ) {
            Ok(result) => result,
            Err(error) => {
                log::error!("AI点歌选择候选失败: {error:#}");
                self.reply(&format!("{}AI点歌识别失败", label))?;
                return Ok(None);
            }
        };
        log::info!(
            "AI点歌候选: raw={} pick={} index={} uri={} score={:.2} reason={}",
            song.keyword,
            candidate.text,
            pick.index,
            candidate.track_ref.key,
            pick.score,
            pick.reason
        );
        let message = format!("{}AI匹配:{},@确认@跳过", label, candidate.text);
        match self.prompt_and_wait_for_decision(&message, false, false, true)? {
            SongRequestDecision::Confirm | SongRequestDecision::Timeout => {}
            SongRequestDecision::Skip => return Ok(None),
            SongRequestDecision::Stopped => return Ok(None),
            SongRequestDecision::SwitchSource | SongRequestDecision::Ai => return Ok(None),
        }
        Ok(Some(ResolvedSongRequest {
            keyword: candidate.text.clone(),
            source: String::new(),
            prefer_accompaniment: song.prefer_accompaniment,
            ai_original_text: song.keyword.clone(),
            track: Some(candidate.playable_track()),
            friend_username: song.friend_username.clone(),
            requester: String::new(),
            console_bypass_dedup: false,
            candidate_snapshot: candidates,
        }))
    }

    fn resolve_and_confirm_song(
        &mut self,
        song: &SongCommand,
    ) -> Result<Option<ResolvedSongRequest>> {
        let Some(request) = self.resolve_song_request(song)? else {
            return Ok(None);
        };
        if request.track.is_none() {
            let source = if request.source.trim().is_empty() {
                "qqmusic"
            } else {
                &request.source
            };
            let picked = match self.port.search_and_pick(
                &request.keyword,
                source,
                request.prefer_accompaniment,
            ) {
                Ok(picked) => picked,
                Err(error) => {
                    self.report_player_search_failure(
                        &request.label(),
                        "点歌候选搜索失败",
                        &error,
                    )?;
                    return Ok(None);
                }
            };
            let Some(picked) = picked else {
                let actions = if self.ai.enabled() {
                    "@换源@AI"
                } else {
                    "@换源"
                };
                let prompt = format!("{}平台无对应歌曲音源,{}", request.label(), actions);
                let decision =
                    self.prompt_and_wait_for_decision(&prompt, true, self.ai.enabled(), true)?;
                match decision {
                    SongRequestDecision::SwitchSource => {
                        let next_source = alternate_music_source(source);
                        return self.resolve_and_confirm_song_with_source(song, next_source);
                    }
                    SongRequestDecision::Ai if self.ai.enabled() => {
                        return self.resolve_and_confirm_song_ai(song);
                    }
                    SongRequestDecision::Confirm
                    | SongRequestDecision::Skip
                    | SongRequestDecision::Timeout
                    | SongRequestDecision::Stopped
                    | SongRequestDecision::Ai => return Ok(None),
                }
            };
            let song_title = picked.candidate.text.clone();
            let track = picked.candidate.playable_track();
            let actions = if self.ai.enabled() {
                "@确认@跳过@换源@AI"
            } else {
                "@确认@跳过@换源"
            };
            let prompt = format!("{}搜索到:{},{}", request.label(), song_title, actions);
            let decision =
                self.prompt_and_wait_for_decision(&prompt, true, self.ai.enabled(), true)?;
            match decision {
                SongRequestDecision::Confirm | SongRequestDecision::Timeout => {
                    return Ok(Some(ResolvedSongRequest {
                        keyword: picked.candidate.text.clone(),
                        source: source.to_string(),
                        prefer_accompaniment: request.prefer_accompaniment,
                        ai_original_text: String::new(),
                        track: Some(track),
                        friend_username: request.friend_username.clone(),
                        requester: request.requester.clone(),
                        console_bypass_dedup: request.console_bypass_dedup,
                        candidate_snapshot: picked.candidate_snapshot,
                    }));
                }
                SongRequestDecision::Skip => {
                    return Ok(None);
                }
                SongRequestDecision::SwitchSource => {
                    let next_source = alternate_music_source(source);
                    return self.resolve_and_confirm_song_with_source(song, next_source);
                }
                SongRequestDecision::Ai if self.ai.enabled() => {
                    return self.resolve_and_confirm_song_ai(song);
                }
                SongRequestDecision::Stopped | SongRequestDecision::Ai => return Ok(None),
            }
        }
        Ok(Some(request))
    }

    fn resolve_and_confirm_song_with_source(
        &mut self,
        song: &SongCommand,
        source: &str,
    ) -> Result<Option<ResolvedSongRequest>> {
        let picked =
            match self
                .port
                .search_and_pick(&song.keyword, source, song.prefer_accompaniment)
            {
                Ok(picked) => picked,
                Err(error) => {
                    self.report_player_search_failure(
                        &song_label(song),
                        "换源后的点歌候选搜索失败",
                        &error,
                    )?;
                    return Ok(None);
                }
            };
        let Some(picked) = picked else {
            let actions = if self.ai.enabled() {
                "@换源@AI"
            } else {
                "@换源"
            };
            let prompt = format!("{}换源后仍无音源,{}", song_label(song), actions);
            let decision =
                self.prompt_and_wait_for_decision(&prompt, true, self.ai.enabled(), true)?;
            match decision {
                SongRequestDecision::SwitchSource => {
                    let next_source = alternate_music_source(source);
                    return self.resolve_and_confirm_song_with_source(song, next_source);
                }
                SongRequestDecision::Ai if self.ai.enabled() => {
                    return self.resolve_and_confirm_song_ai(song);
                }
                SongRequestDecision::Confirm
                | SongRequestDecision::Skip
                | SongRequestDecision::Timeout
                | SongRequestDecision::Stopped
                | SongRequestDecision::Ai => return Ok(None),
            }
        };
        let actions = if self.ai.enabled() {
            "@确认@跳过@换源@AI"
        } else {
            "@确认@跳过@换源"
        };
        let prompt = format!(
            "{}搜索到:{},{}",
            song_label(song),
            picked.candidate.text,
            actions
        );
        let decision = self.prompt_and_wait_for_decision(&prompt, true, self.ai.enabled(), true)?;
        match decision {
            SongRequestDecision::Confirm | SongRequestDecision::Timeout => {
                Ok(Some(ResolvedSongRequest {
                    keyword: picked.candidate.text.clone(),
                    source: source.to_string(),
                    prefer_accompaniment: song.prefer_accompaniment,
                    ai_original_text: String::new(),
                    track: Some(picked.candidate.playable_track()),
                    friend_username: song.friend_username.clone(),
                    requester: String::new(),
                    console_bypass_dedup: false,
                    candidate_snapshot: picked.candidate_snapshot,
                }))
            }
            SongRequestDecision::Skip => Ok(None),
            SongRequestDecision::SwitchSource => {
                let next_source = alternate_music_source(source);
                self.resolve_and_confirm_song_with_source(song, next_source)
            }
            SongRequestDecision::Ai if self.ai.enabled() => self.resolve_and_confirm_song_ai(song),
            SongRequestDecision::Stopped | SongRequestDecision::Ai => Ok(None),
        }
    }

    fn resolve_and_confirm_song_ai(
        &mut self,
        song: &SongCommand,
    ) -> Result<Option<ResolvedSongRequest>> {
        self.resolve_ai_song_request(song, false)
    }

    fn queue_contains_request(&self, request: &ResolvedSongRequest) -> Result<bool> {
        self.port.queue_contains(QueueItem {
            keyword: request.keyword.clone(),
            source: request.source.clone(),
            prefer_accompaniment: request.prefer_accompaniment,
            track: request.track.clone(),
            ..QueueItem::default()
        })
    }

    fn push_queue_request(&self, request: &ResolvedSongRequest) -> Result<SongQueuePushOutcome> {
        if self.song_dedup_limited(request)? {
            log::info!(
                "长时间同歌去重入队拦截: keyword={} uri={}",
                request.keyword,
                request
                    .track
                    .as_ref()
                    .map(|track| track.track_ref.key.to_string())
                    .unwrap_or_default()
            );
            return Ok(SongQueuePushOutcome::DedupLimited);
        }
        let pushed = self.port.push_queue(QueueItem {
            id: 0,
            keyword: request.keyword.clone(),
            source: request.source.clone(),
            prefer_accompaniment: request.prefer_accompaniment,
            ai_original_text: request.ai_original_text.clone(),
            track: request.track.clone(),
            friend_username: request.friend_username.clone(),
            requester: request.requester.clone(),
            dedup_bypass: request.console_bypass_dedup,
            candidate_snapshot: request.candidate_snapshot.clone(),
        })?;
        if pushed.accepted {
            Ok(SongQueuePushOutcome::Added(pushed.size))
        } else {
            Ok(SongQueuePushOutcome::Full)
        }
    }

    fn handle_queue_push_outcome(
        &self,
        context: &SongRequestContext,
        request: &ResolvedSongRequest,
        outcome: SongQueuePushOutcome,
        feedback: QueuePushFeedback,
    ) -> Result<()> {
        match outcome {
            SongQueuePushOutcome::Added(len) => {
                self.log_executed_command(context, &request.final_command(feedback.queued_action))?;
                self.reply(&format!(
                    "{}({}/{}): {}",
                    feedback.queued_prefix, len, self.queue_max_size, request.keyword
                ))?;
            }
            SongQueuePushOutcome::Full => {
                self.log_executed_command(context, &request.final_command(feedback.full_action))?;
                self.reply(feedback.full_reply)?;
            }
            SongQueuePushOutcome::DedupLimited => {
                self.log_executed_command(context, &request.final_command("dedup-limited-queue"))?;
                self.reply(&request.dedup_reject_message())?;
            }
        }
        Ok(())
    }

    fn log_play_request_outcome(
        &self,
        context: &SongRequestContext,
        request: &ResolvedSongRequest,
        result: &PlaybackResult,
    ) -> Result<()> {
        let action = match result.outcome() {
            PlaybackOutcome::Success => "play",
            PlaybackOutcome::ItemScopedFailure => "no-source",
            PlaybackOutcome::QueueBlockingFailure => "play-blocked",
            PlaybackOutcome::DedupLimited => "dedup-limited",
        };
        self.log_executed_command(
            context,
            &request.final_command_for_playback(action, result.final_request()),
        )
    }

    fn song_dedup_limited(&self, request: &ResolvedSongRequest) -> Result<bool> {
        if request.console_bypass_dedup && self.console_bypass_dedup {
            return Ok(false);
        }
        self.port.song_dedup_limited(&request.playback_request())
    }

    fn review_song_candidate(
        &self,
        context: &SongRequestContext,
        request: &ResolvedSongRequest,
    ) -> Result<bool> {
        if !self.song_review.enabled() {
            return Ok(true);
        }
        if context.message_type == "控制台" {
            log::info!(
                "候选歌曲审核跳过: 控制台最高权限免审 command={} uri={}",
                context.raw,
                request
                    .track
                    .as_ref()
                    .map(|track| track.track_ref.key.to_string())
                    .unwrap_or_default()
            );
            return Ok(true);
        }

        let (title, artist) = split_candidate_title_artist(&request.keyword);
        let track_key = request
            .track
            .as_ref()
            .map(|track| track.track_ref.key.clone())
            .ok_or_else(|| anyhow!("候选歌曲审核缺少结构化曲目"))?;
        let candidate = SongReviewCandidate {
            source: request.source.clone(),
            title,
            artist,
            track_key,
            message_type: context.message_type.clone(),
            username: context.username.clone(),
        };
        let decision = self.song_review.review(&candidate);
        let level = song_review_level_text(decision.level);
        let reason = normalized_review_reason(&decision.reason);
        let tags = if decision.tags.is_empty() {
            "无".to_string()
        } else {
            decision.tags.join(",")
        };

        if decision.allowed {
            if decision.failed_open {
                log::warn!(
                    "候选歌曲审核放行: failure_policy=allow attempts={} threshold={} command={} title={} artist={} source={} uri={} reason={}",
                    decision.attempts,
                    decision.threshold,
                    context.raw,
                    candidate.title,
                    candidate.artist,
                    candidate.source,
                    candidate.track_key,
                    reason
                );
            } else {
                log::info!(
                    "候选歌曲审核通过: level={} threshold={} attempts={} command={} title={} artist={} source={} uri={} reason={} tags={}",
                    level,
                    decision.threshold,
                    decision.attempts,
                    context.raw,
                    candidate.title,
                    candidate.artist,
                    candidate.source,
                    candidate.track_key,
                    reason,
                    tags
                );
            }
            return Ok(true);
        }

        log::warn!(
            "候选歌曲审核拒绝: level={} threshold={} attempts={} command={} title={} artist={} source={} uri={} reason={} tags={}",
            level,
            decision.threshold,
            decision.attempts,
            context.raw,
            candidate.title,
            candidate.artist,
            candidate.source,
            candidate.track_key,
            reason,
            tags
        );
        let action = decision.level.map_or_else(
            || "review-reject-failed".to_string(),
            |level| format!("review-reject-level-{level}"),
        );
        self.log_executed_command(context, &request.final_command(&action))?;
        self.reply(&review_reject_reply(
            &reason,
            self.song_review.reply_reason_max_chars(),
        ))?;
        Ok(false)
    }

    fn reply(&self, message: &str) -> Result<()> {
        self.port.reply(message)
    }

    fn prompt_and_wait_for_decision(
        &mut self,
        message: &str,
        allow_switch_source: bool,
        allow_ai: bool,
        default_confirm: bool,
    ) -> Result<SongRequestDecision> {
        self.port.prompt_and_wait_for_decision(
            message,
            allow_switch_source,
            allow_ai,
            default_confirm,
        )
    }

    fn playback_queue(&self) -> Result<Vec<QueueItem>> {
        self.port.playback_queue()
    }

    fn play_request_confirmed(&mut self, request: &ResolvedSongRequest) -> Result<PlaybackResult> {
        self.port.play_confirmed(request)
    }

    fn log_executed_command(
        &self,
        context: &SongRequestContext,
        final_command: &str,
    ) -> Result<()> {
        self.port.log_executed(context, final_command)
    }
}

fn ai_candidate_source(song: &SongCommand) -> &'static str {
    if song.friend_username.trim().is_empty() {
        "qqmusic,netease,bilibili,kugou"
    } else {
        song.source.as_str()
    }
}

fn alternate_music_source(source: &str) -> &'static str {
    if source == SongSource::Netease.as_str() {
        SongSource::QqMusic.as_str()
    } else {
        SongSource::Netease.as_str()
    }
}

fn song_label(song: &SongCommand) -> String {
    source_label(&song.friend_username)
}

fn source_label(username: &str) -> String {
    let username = username.trim();
    if username.is_empty() {
        String::new()
    } else {
        format!("好友{}:", username)
    }
}

fn song_review_level_text(level: Option<u8>) -> String {
    level
        .map(|level| level.to_string())
        .unwrap_or_else(|| "无".to_string())
}

fn normalized_review_reason(reason: &str) -> String {
    let reason = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    if reason.trim().is_empty() {
        "审核服务未给出原因".to_string()
    } else {
        reason
    }
}

fn review_reject_reply(reason: &str, max_chars: usize) -> String {
    let reason = normalized_review_reason(reason);
    let max_chars = max_chars.max(1);
    let shortened = if reason.chars().count() > max_chars {
        format!("{}...", reason.chars().take(max_chars).collect::<String>())
    } else {
        reason
    };
    format!("点歌未通过审核: {shortened}")
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use anyhow::{Result, anyhow};

    use super::*;
    use crate::features::playback::{test_candidate, test_track};
    use crate::features::song_request::SongSource;

    fn application() -> SongRequestApplication {
        SongRequestApplication::with_gateways(
            Arc::new(DisabledAiGateway),
            Arc::new(DisabledReviewGateway),
            20,
            true,
        )
    }

    struct DisabledAiGateway;

    struct IndexedAiGateway {
        index: usize,
    }

    impl SongRequestAiGateway for IndexedAiGateway {
        fn enabled(&self) -> bool {
            true
        }

        fn pick_song_candidate(
            &self,
            _request: &str,
            _prefer_accompaniment: bool,
            _candidates: &[SearchCandidate],
        ) -> Result<AiCandidatePickResult> {
            Ok(AiCandidatePickResult {
                index: self.index,
                reason: "test selection".to_string(),
                score: 0.9,
            })
        }
    }

    impl SongRequestAiGateway for DisabledAiGateway {
        fn enabled(&self) -> bool {
            false
        }

        fn pick_song_candidate(
            &self,
            _request: &str,
            _prefer_accompaniment: bool,
            _candidates: &[SearchCandidate],
        ) -> Result<AiCandidatePickResult> {
            unreachable!("AI is disabled")
        }
    }

    struct AllowingReviewGateway {
        candidates: Mutex<Vec<SongReviewCandidate>>,
    }

    struct DisabledReviewGateway;

    impl SongReviewGateway for DisabledReviewGateway {
        fn enabled(&self) -> bool {
            false
        }

        fn reply_reason_max_chars(&self) -> usize {
            40
        }

        fn review(&self, _candidate: &SongReviewCandidate) -> SongReviewDecision {
            unreachable!("review is disabled")
        }
    }

    impl SongReviewGateway for AllowingReviewGateway {
        fn enabled(&self) -> bool {
            true
        }

        fn reply_reason_max_chars(&self) -> usize {
            40
        }

        fn review(&self, candidate: &SongReviewCandidate) -> SongReviewDecision {
            self.candidates
                .lock()
                .expect("review candidates")
                .push(candidate.clone());
            SongReviewDecision {
                allowed: true,
                level: Some(2),
                threshold: 4,
                reason: "舒缓".to_string(),
                tags: vec!["soft".to_string()],
                attempts: 1,
                failed_open: false,
            }
        }
    }

    fn context() -> SongRequestContext {
        SongRequestContext {
            message_type: "blue".to_string(),
            raw: "@点歌 晴天".to_string(),
            username: "Alice".to_string(),
            user_command: "@点歌 晴天".to_string(),
        }
    }

    fn command() -> SongCommand {
        SongCommand {
            keyword: "晴天".to_string(),
            source: SongSource::QqMusic,
            prefix: "点歌".to_string(),
            prefer_accompaniment: false,
            ai_assisted: false,
            friend_username: String::new(),
        }
    }

    fn stopped_status() -> PlayerStatus {
        PlayerStatus {
            status: "stopped".to_string(),
            current_uri: String::new(),
            name: String::new(),
            singer: String::new(),
            album_name: String::new(),
            lyric_line_text: String::new(),
            duration: 0.0,
            progress: 0.0,
            playback_rate: 1.0,
            volume: 50,
            requester: String::new(),
            ..PlayerStatus::default()
        }
    }

    enum FakeStatus {
        Available(Box<PlayerStatus>),
        Unavailable,
    }

    struct FakePort {
        replies: RefCell<Vec<String>>,
        decision_prompts: RefCell<Vec<String>>,
        decisions: VecDeque<SongRequestDecision>,
        searches: RefCell<VecDeque<Option<PickedCandidate>>>,
        search_sources: RefCell<Vec<String>>,
        queue: RefCell<Vec<QueueItem>>,
        status: FakeStatus,
        should_queue: bool,
        status_matches: bool,
        play_outcome: PlaybackOutcome,
        play_final_request: Option<PlaybackRequest>,
        played: RefCell<Vec<ResolvedSongRequest>>,
        dedup_limited: Cell<bool>,
        logs: RefCell<Vec<String>>,
    }

    impl FakePort {
        fn idle(searches: impl IntoIterator<Item = Option<PickedCandidate>>) -> Self {
            Self {
                replies: RefCell::new(Vec::new()),
                decision_prompts: RefCell::new(Vec::new()),
                decisions: VecDeque::from([SongRequestDecision::Confirm]),
                searches: RefCell::new(searches.into_iter().collect()),
                search_sources: RefCell::new(Vec::new()),
                queue: RefCell::new(Vec::new()),
                status: FakeStatus::Available(Box::new(stopped_status())),
                should_queue: false,
                status_matches: false,
                play_outcome: PlaybackOutcome::Success,
                play_final_request: None,
                played: RefCell::new(Vec::new()),
                dedup_limited: Cell::new(false),
                logs: RefCell::new(Vec::new()),
            }
        }
    }

    impl SongRequestPort for FakePort {
        fn reply(&self, message: &str) -> Result<()> {
            self.replies.borrow_mut().push(message.to_string());
            Ok(())
        }

        fn prompt_and_wait_for_decision(
            &mut self,
            message: &str,
            _allow_switch_source: bool,
            _allow_ai: bool,
            _default_confirm: bool,
        ) -> Result<SongRequestDecision> {
            self.decision_prompts.borrow_mut().push(message.to_string());
            self.reply(message)?;
            Ok(self
                .decisions
                .pop_front()
                .unwrap_or(SongRequestDecision::Timeout))
        }

        fn search_candidates(
            &self,
            _keyword: &str,
            _source: &str,
        ) -> std::result::Result<Option<Vec<SearchCandidate>>, SongSearchFailure> {
            unreachable!("AI is disabled")
        }

        fn search_and_pick(
            &self,
            _keyword: &str,
            source: &str,
            _prefer_accompaniment: bool,
        ) -> std::result::Result<Option<PickedCandidate>, SongSearchFailure> {
            self.search_sources.borrow_mut().push(source.to_string());
            Ok(self.searches.borrow_mut().pop_front().flatten())
        }

        fn playback_queue(&self) -> Result<Vec<QueueItem>> {
            Ok(self.queue.borrow().clone())
        }

        fn queue_contains(&self, item: QueueItem) -> Result<bool> {
            Ok(self
                .queue
                .borrow()
                .iter()
                .any(|queued| queued.track == item.track && queued.keyword == item.keyword))
        }

        fn push_queue(&self, mut item: QueueItem) -> Result<QueuePushOutcome> {
            let mut queue = self.queue.borrow_mut();
            item.id = queue.len() as u64 + 1;
            queue.push(item);
            Ok(QueuePushOutcome {
                accepted: true,
                size: queue.len(),
            })
        }

        fn player_status(&self) -> Result<PlayerStatus> {
            match &self.status {
                FakeStatus::Available(status) => Ok(status.as_ref().clone()),
                FakeStatus::Unavailable => Err(anyhow!("status unavailable")),
            }
        }

        fn should_queue_until_current_song_finished(&self, _status: &PlayerStatus) -> Result<bool> {
            Ok(self.should_queue)
        }

        fn current_status_matches_request(&self, _status: &PlayerStatus) -> Result<bool> {
            Ok(self.status_matches)
        }

        fn play_confirmed(&mut self, request: &ResolvedSongRequest) -> Result<PlaybackResult> {
            self.played.borrow_mut().push(request.clone());
            let requested = request.playback_request();
            let final_request = self.play_final_request.as_ref().unwrap_or(&requested);
            Ok(PlaybackResult::for_test(
                self.play_outcome,
                &requested,
                final_request,
            ))
        }

        fn song_dedup_limited(&self, _request: &PlaybackRequest) -> Result<bool> {
            Ok(self.dedup_limited.get())
        }

        fn log_executed(&self, _context: &SongRequestContext, final_command: &str) -> Result<()> {
            self.logs.borrow_mut().push(final_command.to_string());
            Ok(())
        }
    }

    fn picked(text: &str, uri: &str) -> PickedCandidate {
        let candidate = test_candidate(text, uri);
        PickedCandidate {
            candidate_snapshot: vec![candidate.clone()],
            candidate,
            formatted_candidates: text.to_string(),
        }
    }

    #[test]
    fn ai_candidate_selection_uses_the_returned_index() {
        let candidates = vec![
            test_candidate("same title", "miliastra://track/qqmusic/first"),
            test_candidate("same title", "miliastra://track/netease/second"),
        ];

        let (selected, pick) = select_ai_candidate(
            &IndexedAiGateway { index: 2 },
            "same title",
            false,
            &candidates,
        )
        .expect("indexed selection");

        assert_eq!(pick.index, 2);
        assert_eq!(selected.track_ref.key, candidates[1].track_ref.key);
    }

    #[test]
    fn ai_candidate_selection_rejects_an_out_of_range_index() {
        let candidates = vec![test_candidate(
            "only candidate",
            "miliastra://track/qqmusic/only",
        )];

        let error = select_ai_candidate(
            &IndexedAiGateway { index: 0 },
            "only candidate",
            false,
            &candidates,
        )
        .expect_err("zero is not a one-based candidate index");

        assert!(error.to_string().contains("候选索引: 0"));
    }

    #[test]
    fn confirmed_candidate_plays_when_the_player_is_idle() {
        let mut port =
            FakePort::idle([Some(picked("晴天 - 周杰伦", "miliastra://track/qqmusic/1"))]);

        application()
            .execute(&context(), &command(), &mut port)
            .expect("song request");

        assert_eq!(port.played.borrow().len(), 1);
        assert_eq!(
            port.played.borrow()[0]
                .track
                .as_ref()
                .map(|track| track.track_ref.key.to_string())
                .as_deref(),
            Some("miliastra://track/qqmusic/1")
        );
        assert_eq!(port.played.borrow()[0].requester, "Alice");
        assert_eq!(
            port.played.borrow()[0]
                .playback_request()
                .candidate_snapshot,
            vec![test_candidate(
                "晴天 - 周杰伦",
                "miliastra://track/qqmusic/1"
            )]
        );
        assert!(port.logs.borrow()[0].starts_with("play keyword=晴天 - 周杰伦"));
    }

    #[test]
    fn execution_log_uses_the_final_source_switched_request() {
        let mut port =
            FakePort::idle([Some(picked("晴天 - 周杰伦", "miliastra://track/qqmusic/1"))]);
        port.play_final_request = Some(PlaybackRequest {
            keyword: "晴天 - 周杰伦".to_string(),
            source: "netease".to_string(),
            prefer_accompaniment: false,
            track: Some(test_track("miliastra://track/netease/2", "晴天 - 周杰伦")),
            requester: "Alice".to_string(),
            navigation: crate::features::playback::PlaybackNavigation::Normal,
            candidate_snapshot: Vec::new(),
        });

        application()
            .execute(&context(), &command(), &mut port)
            .expect("song request");

        assert_eq!(
            port.logs.borrow().as_slice(),
            [
                "play keyword=晴天 - 周杰伦 source=netease uri=miliastra://track/netease/2 aiOriginal="
            ]
        );
    }

    #[test]
    fn candidate_prompt_uses_one_prompt_and_decision_operation() {
        let mut port =
            FakePort::idle([Some(picked("晴天 - 周杰伦", "miliastra://track/qqmusic/1"))]);

        application()
            .execute(&context(), &command(), &mut port)
            .expect("song request");

        assert_eq!(
            port.decision_prompts.borrow().as_slice(),
            ["搜索到:晴天 - 周杰伦,@确认@跳过@换源"]
        );
    }

    #[test]
    fn unavailable_player_status_queues_the_confirmed_candidate() {
        let mut port =
            FakePort::idle([Some(picked("晴天 - 周杰伦", "miliastra://track/qqmusic/1"))]);
        port.status = FakeStatus::Unavailable;

        application()
            .execute(&context(), &command(), &mut port)
            .expect("song request");

        assert!(port.played.borrow().is_empty());
        assert_eq!(port.queue.borrow().len(), 1);
        assert_eq!(port.queue.borrow()[0].requester, "Alice");
        assert_eq!(
            port.replies.borrow().last().map(String::as_str),
            Some("状态未知，队列已加入(1/20): 晴天 - 周杰伦")
        );
    }

    #[test]
    fn queue_dedup_rejection_does_not_add_an_item() {
        let mut port =
            FakePort::idle([Some(picked("晴天 - 周杰伦", "miliastra://track/qqmusic/1"))]);
        port.queue.borrow_mut().push(QueueItem {
            id: 1,
            keyword: "其他歌曲".to_string(),
            track: Some(test_track(
                "miliastra://track/qqmusic/other",
                "其他歌曲 - 测试歌手",
            )),
            ..QueueItem::default()
        });
        port.dedup_limited.set(true);

        application()
            .execute(&context(), &command(), &mut port)
            .expect("song request");

        assert_eq!(port.queue.borrow().len(), 1);
        assert_eq!(
            port.replies.borrow().last().map(String::as_str),
            Some("晴天 - 周杰伦近期已播放过,请稍后再点")
        );
        assert!(port.logs.borrow()[0].starts_with("dedup-limited-queue"));
    }

    #[test]
    fn switch_source_searches_the_other_provider_before_playing() {
        let mut port = FakePort::idle([
            None,
            Some(picked("晴天 - 周杰伦", "miliastra://track/netease/2")),
        ]);
        port.decisions = VecDeque::from([
            SongRequestDecision::SwitchSource,
            SongRequestDecision::Confirm,
        ]);

        application()
            .execute(&context(), &command(), &mut port)
            .expect("song request");

        assert_eq!(
            port.search_sources.borrow().as_slice(),
            ["qqmusic", "netease"]
        );
        assert_eq!(port.played.borrow()[0].source, "netease");
    }

    #[test]
    fn approved_review_is_completed_before_unknown_status_queues_the_song() {
        let review = Arc::new(AllowingReviewGateway {
            candidates: Mutex::new(Vec::new()),
        });
        let application = SongRequestApplication::with_gateways(
            Arc::new(DisabledAiGateway),
            review.clone(),
            20,
            true,
        );
        let mut port =
            FakePort::idle([Some(picked("晴天 - 周杰伦", "miliastra://track/qqmusic/1"))]);
        port.status = FakeStatus::Unavailable;

        application
            .execute(&context(), &command(), &mut port)
            .expect("reviewed song request");

        let candidates = review.candidates.lock().expect("review candidates");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].title, "晴天");
        assert_eq!(port.queue.borrow().len(), 1);
        assert!(port.logs.borrow()[0].starts_with("queue-status-unknown"));
    }

    #[test]
    fn search_failures_have_distinct_safe_user_messages() {
        for (failure, expected) in [
            (SongSearchFailure::Busy, "歌曲搜索繁忙，请稍后再试"),
            (
                SongSearchFailure::Unavailable("stopped".to_string()),
                "歌曲搜索服务暂不可用，请稍后再试",
            ),
            (
                SongSearchFailure::Backend("failed".to_string()),
                "歌曲搜索后端失败，请稍后再试",
            ),
            (
                SongSearchFailure::Unexpected("invalid".to_string()),
                "歌曲搜索后端返回异常，请稍后再试",
            ),
        ] {
            assert_eq!(failure.user_message(), expected);
            assert!(!failure.user_message().contains("无音源"));
        }
    }

    #[test]
    fn decision_parser_is_case_insensitive_and_ignores_its_own_feedback() {
        assert_eq!(
            SongRequestDecision::parse("用户：@ai"),
            Some(SongRequestDecision::Ai)
        );
        assert_eq!(
            SongRequestDecision::parse("用户：@确认！"),
            Some(SongRequestDecision::Confirm)
        );
        assert!(SongRequestDecision::is_feedback_text(
            "搜索到:晴天,@确认@跳过@换源"
        ));
    }
}
