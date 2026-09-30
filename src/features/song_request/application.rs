use std::fmt::{Display, Formatter};
use std::sync::Arc;

use anyhow::{Result, anyhow};
use miliastra_playback::PlayableTrack;

use super::{
    AiCandidatePickResult, AiClient, SongCommand, SongReviewCandidate, SongReviewClient,
    SongReviewDecision, SongSource,
};
use super::{CandidateEligibility, PickedCandidate, SearchCandidate};
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
    /// 调用方是否具备好友及以上权限：好友私聊、已映射的好友/管理员/主人，
    /// 以及控制面板发起的点歌都算；B站 音源与本地曲库兜底以此为门槛。
    pub(crate) friend_or_above: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SongRequestDecision {
    Confirm,
    Skip,
    SwitchSource,
    Ai,
    Timeout,
    Stopped,
    Cancelled,
    Select,
    SelectIndex(usize),
    /// 查看/选用本地曲库推荐；与在线候选的序号空间分开，避免误选。
    LocalLibrary,
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
        } else if command_text
            .strip_prefix("@选择")
            .is_some_and(|rest| decision_boundary(rest.chars().next()))
        {
            Some(Self::Select)
        } else if command_text
            .strip_prefix("@本地")
            .is_some_and(|rest| decision_boundary(rest.chars().next()))
        {
            Some(Self::LocalLibrary)
        } else if let Some(rest) = command_text.strip_prefix('@') {
            let index = rest.parse::<usize>().ok()?;
            (1..=5).contains(&index).then_some(Self::SelectIndex(index))
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
            "AI理解搜索词:",
            "二次搜索失败，保留首次搜索结果",
            "AI点歌未启用",
            "AI点歌识别失败",
            "本地曲库推荐",
            "本地曲库没有更合适的歌曲",
            "选择本地曲库歌曲",
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

    /// allow_local 为 false 时 @本地 会被忽略（继续等待其他确认命令）。
    fn prompt_and_wait_for_decision_batch(
        &mut self,
        messages: &[String],
        allow_switch_source: bool,
        allow_ai: bool,
        allow_local: bool,
        default_confirm: bool,
    ) -> Result<SongRequestDecision>;

    fn search_candidates(
        &self,
        keyword: &str,
        source: &str,
    ) -> std::result::Result<Option<Vec<SearchCandidate>>, SongSearchFailure>;

    /// 本地曲库候选；AI 点歌与普通点歌都会调用，供在线匹配偏低时推荐。
    ///
    /// 本地检索跨平台进行，不受在线 source 限制；`allow_bilibili` 表示本次请求
    /// 具备好友及以上权限，只有它才允许把 B站 音源纳入本地兜底。
    fn search_library_candidates(
        &self,
        keyword: &str,
        allow_bilibili: bool,
    ) -> Result<Vec<SearchCandidate>>;

    /// 在线候选是否被关键词字面命中：与曲库检索使用同一套匹配规则。
    /// 普通点歌据此判断在线结果是否理想，不理想且本地有命中时才追加本地推荐。
    fn online_candidate_matches_keyword(&self, keyword: &str, candidate: &SearchCandidate) -> bool;

    /// 在线匹配偏低时是否额外给出本地曲库推荐；默认开启。
    fn local_recommend(&self) -> bool {
        true
    }

    /// 在线候选分数低于该值才去比较本地曲库；本地分数必须严格更高才会推荐。
    fn local_recommend_min_score(&self) -> f64 {
        0.6
    }

    fn search_and_pick(
        &self,
        keyword: &str,
        source: &str,
        prefer_accompaniment: bool,
    ) -> std::result::Result<Option<PickedCandidate>, SongSearchFailure>;

    fn playback_queue(&self) -> Result<Vec<QueueItem>>;
    fn queue_contains(&self, item: QueueItem) -> Result<bool>;
    fn push_queue(&self, item: QueueItem) -> Result<QueuePushOutcome>;
    fn preload_track(&self, track: &PlayableTrack) -> Result<()>;
    fn player_status(&self) -> Result<PlayerStatus>;
    fn should_queue_until_current_song_finished(&self, status: &PlayerStatus) -> Result<bool>;
    fn current_status_matches_request(&self, status: &PlayerStatus) -> Result<bool>;
    fn play_confirmed(&mut self, request: &ResolvedSongRequest) -> Result<PlaybackResult>;
    fn song_dedup_limited(&self, request: &PlaybackRequest) -> Result<bool>;
    fn log_executed(&self, context: &SongRequestContext, final_command: &str) -> Result<()>;
}

pub(crate) trait SongRequestAiGateway: Send + Sync {
    fn enabled(&self) -> bool;
    fn rewrite_song_search(
        &self,
        _request: &str,
        _prefer_accompaniment: bool,
        _candidates: &[SearchCandidate],
    ) -> Result<Option<String>> {
        Ok(None)
    }
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
    fn rewrite_song_search(
        &self,
        request: &str,
        prefer_accompaniment: bool,
        candidates: &[SearchCandidate],
    ) -> Result<Option<String>> {
        AiClient::rewrite_song_search(self, request, prefer_accompaniment, candidates)
    }
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
            queue_item_id: None,
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
            friend_or_above: context.friend_or_above,
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
    /// 本次请求是否具备好友及以上权限。
    friend_or_above: bool,
    port: &'a mut dyn SongRequestPort,
}

impl SongRequestExecution<'_> {
    fn execute_song_request_intent(
        &mut self,
        context: &SongRequestContext,
        song: &SongCommand,
    ) -> Result<()> {
        if self.queue_is_full(context)? {
            return Ok(());
        }
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

    fn queue_is_full(&self, context: &SongRequestContext) -> Result<bool> {
        let queue = self.port.playback_queue()?;
        if queue.len() < self.queue_max_size {
            return Ok(false);
        }
        self.log_executed_command(context, "queue-full-early")?;
        self.reply(QUEUE_PUSH_FEEDBACK.full_reply)?;
        Ok(true)
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

    fn search_ai_candidates(
        &self,
        keyword: &str,
        source: &str,
        allow_bilibili: bool,
    ) -> std::result::Result<SongSearchCandidates, SongSearchFailure> {
        let local = match self.port.search_library_candidates(keyword, allow_bilibili) {
            Ok(candidates) => candidates,
            Err(error) => {
                log::warn!("AI点歌曲库检索失败，继续平台搜索: {error:#}");
                Vec::new()
            }
        };
        let online = match self.port.search_candidates(keyword, source) {
            Ok(candidates) => candidates.unwrap_or_default(),
            Err(error) if local.is_empty() => return Err(error),
            Err(error) => {
                log::warn!("AI点歌平台搜索失败，使用曲库候选: {error}");
                Vec::new()
            }
        };
        Ok(SongSearchCandidates::from_sources(local, online))
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

        let search_source = ai_candidate_source(song, self.friend_or_above);
        let allow_bilibili = self.friend_or_above;
        let mut candidates =
            match self.search_ai_candidates(&song.keyword, search_source, allow_bilibili) {
                Ok(candidates) => candidates,
                Err(error) => {
                    self.report_player_search_failure(&label, "AI点歌搜索候选失败", &error)?;
                    return Ok(None);
                }
            };
        let usable: Vec<_> = candidates
            .merged
            .iter()
            .filter(|candidate| {
                matches!(
                    candidate.eligibility,
                    CandidateEligibility::Eligible | CandidateEligibility::Unknown
                )
            })
            .cloned()
            .collect();
        // 版权或账号限制不是点歌语义错误，不能借改写绕过限制。
        if candidates.merged.is_empty() || !usable.is_empty() {
            match self
                .ai
                .rewrite_song_search(&song.keyword, song.prefer_accompaniment, &usable)
            {
                Ok(Some(query)) => {
                    if let Some(mut query) =
                        super::ai::validated_search_rewrite(&song.keyword, &query)
                    {
                        let lower = query.to_lowercase();
                        if song.prefer_accompaniment
                            && ![
                                "伴奏",
                                "伴唱",
                                "instrumental",
                                "karaoke",
                                "inst.",
                                "ktv",
                                "minus one",
                            ]
                            .iter()
                            .any(|term| lower.contains(term))
                        {
                            query.push_str(" 伴奏");
                        }
                        if let Some(query) =
                            super::ai::validated_search_rewrite(&song.keyword, &query)
                        {
                            self.reply(&format!("{}AI理解搜索词:{}，再次搜索", label, query))?;
                            match self.search_ai_candidates(&query, search_source, allow_bilibili) {
                                Ok(second) => {
                                    candidates =
                                        SongSearchCandidates::merge_rounds(candidates, second)
                                }
                                Err(error) => {
                                    log::warn!("AI二次搜索失败，保留首次候选: {error}");
                                    self.reply(&format!(
                                        "{}二次搜索失败，保留首次搜索结果",
                                        label
                                    ))?;
                                }
                            }
                        }
                    }
                }
                Ok(None) => {}
                Err(error) => log::warn!("AI点歌语义分析失败，沿用首次搜索结果: {error:#}"),
            }
        }
        candidates.merged.retain(|candidate| {
            matches!(
                candidate.eligibility,
                CandidateEligibility::Eligible | CandidateEligibility::Unknown
            )
        });
        if candidates.merged.is_empty() {
            self.reply(&format!("{}平台无对应歌曲音源", label))?;
            return Ok(None);
        }

        let (candidate, pick) = match select_ai_candidate(
            self.ai,
            &song.keyword,
            song.prefer_accompaniment,
            &candidates.merged,
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
        // 在线分数偏低时才比较本地曲库：本次选中的是在线候选就直接用它的分数，
        // 否则单独给在线候选打一次分，保证两边是同一套打分口径。
        let picked_is_online = candidates
            .online
            .iter()
            .any(|item| item.track_ref.key == candidate.track_ref.key);
        let online_score = if picked_is_online {
            Some(pick.score)
        } else if candidates.online.is_empty() {
            None
        } else {
            let online: Vec<SearchCandidate> = candidates
                .online
                .iter()
                .filter(|item| is_playable_candidate(item))
                .cloned()
                .collect();
            match self
                .ai
                .pick_song_candidate(&song.keyword, song.prefer_accompaniment, &online)
            {
                Ok(result) => Some(result.score),
                Err(error) => {
                    log::warn!("在线候选打分失败，无法比较本地推荐: {error:#}");
                    None
                }
            }
        };
        let local_offer = online_score.and_then(|online_score| {
            self.local_library_offer(
                &song.keyword,
                song.prefer_accompaniment,
                &candidates,
                online_score,
            )
        });
        let selection = selection_candidates(&candidates.online, &candidates.local);
        // @本地 只出现在下面那条本地推荐里，不在这里重复列出。
        let mut messages = vec![format!(
            "{}AI匹配:{},@确认@跳过@选择",
            label, candidate.text
        )];
        if let Some(offer) = &local_offer {
            messages.push(local_library_line(&label, &offer[0]));
        }
        // 已经给出本地推荐时超时按跳过处理：在线分数本来就低，不能替用户自动播放。
        let decision = self.prompt_candidate_decision(
            &messages,
            &selection,
            false,
            false,
            local_offer.is_some(),
            local_offer.is_none(),
        )?;
        let candidate = match decision {
            SongRequestDecision::Confirm | SongRequestDecision::Timeout => candidate,
            SongRequestDecision::SelectIndex(index) => match selection.get(index - 1).cloned() {
                Some(candidate) => candidate,
                None => return Ok(None),
            },
            SongRequestDecision::LocalLibrary => {
                let Some(offer) = local_offer else {
                    return Ok(None);
                };
                let Some(selected) = self.choose_local_library_candidate(&offer)? else {
                    return Ok(None);
                };
                return Ok(Some(ResolvedSongRequest {
                    keyword: selected.text.clone(),
                    source: String::new(),
                    prefer_accompaniment: song.prefer_accompaniment,
                    ai_original_text: song.keyword.clone(),
                    track: Some(selected.playable_track()),
                    friend_username: song.friend_username.clone(),
                    requester: String::new(),
                    console_bypass_dedup: false,
                    candidate_snapshot: vec![selected],
                }));
            }
            SongRequestDecision::Skip
            | SongRequestDecision::Stopped
            | SongRequestDecision::Select
            | SongRequestDecision::Cancelled
            | SongRequestDecision::SwitchSource
            | SongRequestDecision::Ai => return Ok(None),
        };
        Ok(Some(ResolvedSongRequest {
            keyword: candidate.text.clone(),
            source: String::new(),
            prefer_accompaniment: song.prefer_accompaniment,
            ai_original_text: song.keyword.clone(),
            track: Some(candidate.playable_track()),
            friend_username: song.friend_username.clone(),
            requester: String::new(),
            console_bypass_dedup: false,
            candidate_snapshot: selection,
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
                // 平台没有可播候选时给出本地曲库推荐。这是与在线选歌分开的独立流程，
                // 超时语义为「跳过」，绝不替用户自动播放本地候选。
                match self.prompt_without_online_candidate(
                    &request.label(),
                    &format!("{}平台无对应歌曲音源", request.label()),
                    &request.keyword,
                    self.friend_or_above,
                    true,
                )? {
                    NoCandidateOutcome::Local(candidate) => {
                        return Ok(Some(ResolvedSongRequest {
                            keyword: candidate.text.clone(),
                            source: source.to_string(),
                            prefer_accompaniment: request.prefer_accompaniment,
                            ai_original_text: String::new(),
                            track: Some(candidate.playable_track()),
                            friend_username: request.friend_username.clone(),
                            requester: request.requester.clone(),
                            console_bypass_dedup: request.console_bypass_dedup,
                            candidate_snapshot: vec![candidate],
                        }));
                    }
                    NoCandidateOutcome::SwitchSource => {
                        let next_source = alternate_music_source(source);
                        return self.resolve_and_confirm_song_with_source(song, next_source);
                    }
                    NoCandidateOutcome::Ai => return self.resolve_and_confirm_song_ai(song),
                    NoCandidateOutcome::Skip => return Ok(None),
                }
            };
            // 在线候选没有被关键词字面命中（平台只给了近似结果）而本地库有命中时，
            // 按 AI 路径同一口径追加一条本地推荐，让普通点歌也能用上本地记录。
            let local_offer = self.plain_local_offer_after_online_pick(&request.keyword, &picked);
            let song_title = picked.candidate.text.clone();
            let actions = if self.ai.enabled() {
                "@确认@跳过@换源@AI@选择"
            } else {
                "@确认@跳过@换源@选择"
            };
            let mut messages = vec![format!(
                "{}搜索到:{},{}",
                request.label(),
                song_title,
                actions
            )];
            if let Some(candidate) = local_offer.first() {
                messages.push(local_library_line(&request.label(), candidate));
            }
            let decision = self.prompt_candidate_decision(
                &messages,
                &picked.candidate_snapshot,
                true,
                self.ai.enabled(),
                !local_offer.is_empty(),
                local_offer.is_empty(),
            )?;
            let selected = match decision {
                SongRequestDecision::SelectIndex(index) => {
                    let Some(candidate) = picked.candidate_snapshot.get(index - 1).cloned() else {
                        return Ok(None);
                    };
                    candidate
                }
                SongRequestDecision::Confirm | SongRequestDecision::Timeout => picked.candidate,
                _ => {
                    return match decision {
                        SongRequestDecision::LocalLibrary if !local_offer.is_empty() => {
                            let Some(selected) =
                                self.choose_local_library_candidate(&local_offer)?
                            else {
                                return Ok(None);
                            };
                            return Ok(Some(ResolvedSongRequest {
                                keyword: selected.text.clone(),
                                source: String::new(),
                                prefer_accompaniment: request.prefer_accompaniment,
                                ai_original_text: String::new(),
                                track: Some(selected.playable_track()),
                                friend_username: request.friend_username.clone(),
                                requester: request.requester.clone(),
                                console_bypass_dedup: request.console_bypass_dedup,
                                candidate_snapshot: vec![selected],
                            }));
                        }
                        SongRequestDecision::Skip
                        | SongRequestDecision::Stopped
                        | SongRequestDecision::Select
                        | SongRequestDecision::Cancelled
                        | SongRequestDecision::LocalLibrary => Ok(None),
                        SongRequestDecision::SwitchSource => {
                            let next_source = alternate_music_source(source);
                            self.resolve_and_confirm_song_with_source(song, next_source)
                        }
                        SongRequestDecision::Ai if self.ai.enabled() => {
                            self.resolve_and_confirm_song_ai(song)
                        }
                        SongRequestDecision::Ai => Ok(None),
                        SongRequestDecision::Confirm
                        | SongRequestDecision::Timeout
                        | SongRequestDecision::SelectIndex(_) => unreachable!(),
                    };
                }
            };
            return Ok(Some(ResolvedSongRequest {
                keyword: selected.text.clone(),
                source: source.to_string(),
                prefer_accompaniment: request.prefer_accompaniment,
                ai_original_text: String::new(),
                track: Some(selected.playable_track()),
                friend_username: request.friend_username.clone(),
                requester: request.requester.clone(),
                console_bypass_dedup: request.console_bypass_dedup,
                candidate_snapshot: picked.candidate_snapshot,
            }));
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
            // 换源后仍无音源时同样给出本地曲库推荐；本地推荐是独立流程，
            // 超时按「跳过」处理，不会自动播放。
            match self.prompt_without_online_candidate(
                &song_label(song),
                &format!("{}换源后仍无音源", song_label(song)),
                &song.keyword,
                self.friend_or_above,
                true,
            )? {
                NoCandidateOutcome::Local(candidate) => {
                    return Ok(Some(ResolvedSongRequest {
                        keyword: candidate.text.clone(),
                        source: source.to_string(),
                        prefer_accompaniment: song.prefer_accompaniment,
                        ai_original_text: String::new(),
                        track: Some(candidate.playable_track()),
                        friend_username: song.friend_username.clone(),
                        requester: String::new(),
                        console_bypass_dedup: false,
                        candidate_snapshot: vec![candidate],
                    }));
                }
                NoCandidateOutcome::SwitchSource => {
                    let next_source = alternate_music_source(source);
                    return self.resolve_and_confirm_song_with_source(song, next_source);
                }
                NoCandidateOutcome::Ai => return self.resolve_and_confirm_song_ai(song),
                NoCandidateOutcome::Skip => return Ok(None),
            }
        };
        let actions = if self.ai.enabled() {
            "@确认@跳过@换源@AI@选择"
        } else {
            "@确认@跳过@换源@选择"
        };
        let prompt = format!(
            "{}搜索到:{},{}",
            song_label(song),
            picked.candidate.text,
            actions
        );
        let decision = self.prompt_candidate_decision(
            &[prompt],
            &picked.candidate_snapshot,
            true,
            self.ai.enabled(),
            false,
            true,
        )?;
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
            SongRequestDecision::SelectIndex(index) => {
                let candidate = picked
                    .candidate_snapshot
                    .get(index - 1)
                    .ok_or_else(|| anyhow!("歌曲候选索引超出范围: {index}"))?;
                Ok(Some(ResolvedSongRequest {
                    keyword: candidate.text.clone(),
                    source: source.to_string(),
                    prefer_accompaniment: song.prefer_accompaniment,
                    ai_original_text: String::new(),
                    track: Some(candidate.playable_track()),
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
            SongRequestDecision::Stopped
            | SongRequestDecision::Ai
            | SongRequestDecision::Select
            | SongRequestDecision::Cancelled
            | SongRequestDecision::LocalLibrary => Ok(None),
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
            // 只有队首（入队后队列里唯一曲目）才立即解析音源：排在后面的曲目
            // 交给播放侧的下一首预加载，避免为长时间等待的曲目提前取流。
            if pushed.size == 1
                && let Some(track) = &request.track
                && let Err(error) = self.port.preload_track(track)
            {
                log::debug!("点歌入队后预加载音源失败，播放时将重试: {error:#}");
            }
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

        let track = request
            .track
            .as_ref()
            .ok_or_else(|| anyhow!("候选歌曲审核缺少结构化曲目"))?;
        let candidate = SongReviewCandidate {
            source: track.track_ref.key.provider.to_string(),
            title: track.metadata.title.clone(),
            artist: track.metadata.artists.join(" / "),
            duration_ms: track.metadata.duration_ms,
            track_key: track.track_ref.key.clone(),
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

    /// 展示候选并等待选择。messages 是首批消息（在线提示，必要时附带本地推荐行）；
    /// default_confirm=false 时超时按跳过处理，用于本地推荐场景。
    fn prompt_candidate_decision(
        &mut self,
        messages: &[String],
        candidates: &[SearchCandidate],
        allow_switch_source: bool,
        allow_ai: bool,
        allow_local: bool,
        default_confirm: bool,
    ) -> Result<SongRequestDecision> {
        let mut decision = self.port.prompt_and_wait_for_decision_batch(
            messages,
            allow_switch_source,
            allow_ai,
            allow_local,
            default_confirm,
        )?;
        let candidate_count = candidates.len().min(5);
        let choices = candidates
            .iter()
            .take(5)
            .enumerate()
            .map(|(index, candidate)| format!("@{} {}", index + 1, candidate.selection_text()))
            .collect::<Vec<_>>();
        let switch_source_action = if allow_switch_source {
            "，或@换源重新搜索"
        } else {
            ""
        };
        let prompt = format!(
            "请输入@1至@{}选择歌曲{}，或@跳过",
            candidate_count, switch_source_action,
        );
        let mut choice_messages = choices;
        choice_messages.push(prompt);
        loop {
            match decision {
                SongRequestDecision::Select => {}
                SongRequestDecision::SelectIndex(index)
                    if !(1..=candidate_count).contains(&index) => {}
                _ => return Ok(decision),
            }
            decision = self.port.prompt_and_wait_for_decision_batch(
                &choice_messages,
                allow_switch_source,
                false,
                false,
                false,
            )?;
        }
    }

    /// 普通点歌拿到在线候选后的本地推荐：只在线候选没有被关键词字面命中
    /// （平台给的是近似结果）且本地库存在命中的其它记录时给出，避免打扰正常点歌。
    fn plain_local_offer_after_online_pick(
        &mut self,
        keyword: &str,
        picked: &PickedCandidate,
    ) -> Vec<SearchCandidate> {
        if !self.port.local_recommend()
            || self
                .port
                .online_candidate_matches_keyword(keyword, &picked.candidate)
        {
            return Vec::new();
        }
        let local = match self
            .port
            .search_library_candidates(keyword, self.friend_or_above)
        {
            Ok(candidates) => candidates,
            Err(error) => {
                log::warn!("本地曲库检索失败，无法给出本地推荐: {error:#}");
                return Vec::new();
            }
        };
        local_library_choices(&local, picked.candidate_snapshot.len())
            .into_iter()
            .filter(|candidate| candidate.track_ref.key != picked.candidate.track_ref.key)
            .collect()
    }

    /// 普通点歌的本地曲库候选。在线没有可播候选时无事可比较，
    /// 因此不做分数门槛，直接用曲库检索的排序结果。
    fn plain_local_offer(&mut self, keyword: &str, allow_bilibili: bool) -> Vec<SearchCandidate> {
        if !self.port.local_recommend() {
            return Vec::new();
        }
        match self.port.search_library_candidates(keyword, allow_bilibili) {
            Ok(candidates) => local_library_choices(&candidates, 0),
            Err(error) => {
                log::warn!("本地曲库检索失败，无法给出本地推荐: {error:#}");
                Vec::new()
            }
        }
    }

    /// AI 点歌的本地曲库推荐：在线分数偏低时用同一套 AI 打分比较本地候选，
    /// 只有本地分数严格高于在线分数才推荐。返回 None 表示不推荐。
    fn local_library_offer(
        &mut self,
        keyword: &str,
        prefer_accompaniment: bool,
        candidates: &SongSearchCandidates,
        online_score: f64,
    ) -> Option<Vec<SearchCandidate>> {
        if !self.port.local_recommend() || online_score >= self.port.local_recommend_min_score() {
            return None;
        }
        let local: Vec<SearchCandidate> = candidates
            .local
            .iter()
            .filter(|candidate| is_playable_candidate(candidate))
            .cloned()
            .collect();
        if local.is_empty() {
            return None;
        }
        let online_count = candidates
            .online
            .iter()
            .filter(|item| is_playable_candidate(item))
            .count();
        match self
            .ai
            .pick_song_candidate(keyword, prefer_accompaniment, &local)
        {
            Ok(local_pick) if local_pick.score > online_score => {
                log::info!(
                    "本地曲库推荐: 在线score={online_score:.2} 本地score={:.2}",
                    local_pick.score
                );
                Some(local_library_choices(&local, online_count))
            }
            Ok(local_pick) => {
                log::info!(
                    "本地曲库不推荐: 在线score={online_score:.2} 本地score={:.2}",
                    local_pick.score
                );
                None
            }
            Err(error) => {
                log::warn!("本地曲库候选打分失败，不推荐本地结果: {error:#}");
                None
            }
        }
    }

    /// 独立的本地曲库推荐流程：按序号展示本地候选并等待用户选择。
    /// 超时语义为「跳过」（default_confirm=false）：曲库元数据可能已经过期，
    /// 绝不能替用户自动播放本地候选。
    fn choose_local_library_candidate(
        &mut self,
        choices: &[SearchCandidate],
    ) -> Result<Option<SearchCandidate>> {
        if choices.is_empty() {
            // 没有本地推荐时 @本地 由决策层直接忽略，这里只做兜底。
            return Ok(None);
        }
        let count = choices.len().min(SELECTION_SLOTS);
        if count == 1 {
            // 只有一首时直接选用，省一次往返；用户仍需主动发 @本地 才会走到这里。
            return Ok(choices.first().cloned());
        }
        let mut messages: Vec<String> = choices
            .iter()
            .take(count)
            .enumerate()
            .map(|(index, candidate)| format!("@{} {}", index + 1, candidate.text))
            .collect();
        messages.push(format!("请输入@1至@{}选择本地曲库歌曲，或@跳过", count));
        loop {
            match self
                .port
                .prompt_and_wait_for_decision_batch(&messages, false, false, false, false)?
            {
                SongRequestDecision::SelectIndex(index) if (1..=count).contains(&index) => {
                    return Ok(choices.get(index - 1).cloned());
                }
                // @选择 或越界序号：重发列表继续等待；其余（跳过、超时、停止）一律放弃。
                SongRequestDecision::Select | SongRequestDecision::SelectIndex(_) => continue,
                _ => return Ok(None),
            }
        }
    }

    /// 平台（或换源后）没有可播候选时的统一处理：追加本地推荐行并等待用户决定。
    /// 本地推荐是独立流程，超时按「跳过」处理，不会自动播放。
    fn prompt_without_online_candidate(
        &mut self,
        label: &str,
        message: &str,
        keyword: &str,
        allow_bilibili: bool,
        allow_switch_source: bool,
    ) -> Result<NoCandidateOutcome> {
        let offer = self.plain_local_offer(keyword, allow_bilibili);
        let mut actions = String::new();
        if allow_switch_source {
            actions.push_str("@换源");
        }
        if self.ai.enabled() {
            actions.push_str("@AI");
        }
        // @本地 只出现在下面那条本地推荐里，不在这里重复列出。
        let mut messages = vec![format!("{message},{actions}")];
        let allow_local = !offer.is_empty();
        if let Some(candidate) = offer.first() {
            messages.push(local_library_line(label, candidate));
        }
        let decision = self.port.prompt_and_wait_for_decision_batch(
            &messages,
            allow_switch_source,
            self.ai.enabled(),
            allow_local,
            true,
        )?;
        match decision {
            SongRequestDecision::LocalLibrary => {
                match self.choose_local_library_candidate(&offer)? {
                    Some(candidate) => Ok(NoCandidateOutcome::Local(candidate)),
                    None => Ok(NoCandidateOutcome::Skip),
                }
            }
            SongRequestDecision::SwitchSource if allow_switch_source => {
                Ok(NoCandidateOutcome::SwitchSource)
            }
            SongRequestDecision::Ai if self.ai.enabled() => Ok(NoCandidateOutcome::Ai),
            _ => Ok(NoCandidateOutcome::Skip),
        }
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

/// 同一曲目以在线结果的当前可播性和元数据为准，不能用本地 Unknown 覆盖限制。
/// 平台无可用候选时的处理结果。
enum NoCandidateOutcome {
    Local(SearchCandidate),
    SwitchSource,
    Ai,
    Skip,
}

/// 序号选择窗口的候选数量上限，与 @1～@5 保持一致。
const SELECTION_SLOTS: usize = 5;

/// 一次搜索得到的候选：在线平台结果与本地曲库结果分开保留。
/// 分开是为了在在线分数偏低时单独给本地候选打分并比较，两边互不干扰。
struct SongSearchCandidates {
    merged: Vec<SearchCandidate>,
    online: Vec<SearchCandidate>,
    local: Vec<SearchCandidate>,
}

impl SongSearchCandidates {
    fn from_sources(local: Vec<SearchCandidate>, online: Vec<SearchCandidate>) -> Self {
        let merged = merge_ai_search_candidates(local.clone(), online.clone());
        Self {
            merged,
            online,
            local,
        }
    }

    /// 二次搜索后按来源分别合并两轮结果，保持与整体列表一致的取舍规则。
    fn merge_rounds(previous: Self, next: Self) -> Self {
        Self {
            merged: merge_ai_search_rounds(previous.merged, next.merged),
            online: merge_ai_search_rounds(previous.online, next.online),
            local: merge_ai_search_rounds(previous.local, next.local),
        }
    }
}

fn is_playable_candidate(candidate: &SearchCandidate) -> bool {
    matches!(
        candidate.eligibility,
        CandidateEligibility::Eligible | CandidateEligibility::Unknown
    )
}

/// 本地推荐可选项：在线样本足够多（≥5）时只给 1 个本地最高分候选，
/// 在线不足时用本地候选补足到 5 个。
fn local_library_choices(local: &[SearchCandidate], online_count: usize) -> Vec<SearchCandidate> {
    let slots = if online_count >= SELECTION_SLOTS {
        1
    } else {
        SELECTION_SLOTS.saturating_sub(online_count).max(1)
    };
    local
        .iter()
        .filter(|candidate| is_playable_candidate(candidate))
        .take(slots)
        .cloned()
        .map(mark_library_candidate)
        .collect()
}

/// 标注候选来自本地曲库，消息里据此与在线结果区分。
fn mark_library_candidate(mut candidate: SearchCandidate) -> SearchCandidate {
    if !candidate.text.contains("曲库]") {
        candidate.text.push_str(" [曲库]");
    }
    candidate
}

/// @1～@5 的选择列表：在线候选在前，本地候选只补空位，同键只保留一次。
fn selection_candidates(
    online: &[SearchCandidate],
    local: &[SearchCandidate],
) -> Vec<SearchCandidate> {
    let online: Vec<SearchCandidate> = online
        .iter()
        .filter(|candidate| is_playable_candidate(candidate))
        .cloned()
        .collect();
    let local_choices = local_library_choices(local, online.len());
    let online_slots = if online.len() >= SELECTION_SLOTS {
        SELECTION_SLOTS.saturating_sub(local_choices.len().min(1))
    } else {
        online.len()
    };
    let mut seen = std::collections::HashSet::new();
    let mut list = Vec::with_capacity(SELECTION_SLOTS);
    // 本地候选仍排在前面（与合并顺序一致），在线候选补足剩余名额。
    for candidate in local_choices
        .into_iter()
        .chain(online.into_iter().take(online_slots))
    {
        if list.len() >= SELECTION_SLOTS {
            break;
        }
        if seen.insert(candidate.track_ref.key.clone()) {
            list.push(candidate);
        }
    }
    list
}

/// 在线匹配偏低时追加的本地推荐消息；不展示任何匹配分数。
fn local_library_line(label: &str, candidate: &SearchCandidate) -> String {
    format!(
        "{label}在线没找到合适的,本地曲库推荐:{},@本地",
        candidate.text
    )
}

fn merge_ai_search_candidates(
    local: Vec<SearchCandidate>,
    online: Vec<SearchCandidate>,
) -> Vec<SearchCandidate> {
    let mut seen = std::collections::HashSet::new();
    let mut merged = Vec::new();
    for local_candidate in local {
        let mut candidate = online
            .iter()
            .find(|candidate| candidate.track_ref.key == local_candidate.track_ref.key)
            .cloned()
            .unwrap_or(local_candidate);
        if !candidate.text.contains("曲库]") {
            candidate.text.push_str(" [曲库]");
        }
        if seen.insert(candidate.track_ref.key.clone()) {
            merged.push(candidate);
        }
    }
    for candidate in online {
        if seen.insert(candidate.track_ref.key.clone()) {
            merged.push(candidate);
        }
    }
    merged
}

/// 二次结果优先进入模型可见窗口；任一轮明确不可播的曲目都不能复活。
fn merge_ai_search_rounds(
    first: Vec<SearchCandidate>,
    second: Vec<SearchCandidate>,
) -> Vec<SearchCandidate> {
    let blocked: std::collections::HashSet<_> = first
        .iter()
        .chain(second.iter())
        .filter(|item| {
            !matches!(
                item.eligibility,
                CandidateEligibility::Eligible | CandidateEligibility::Unknown
            )
        })
        .map(|item| item.track_ref.key.clone())
        .collect();
    let mut seen = std::collections::HashSet::new();
    second
        .iter()
        .chain(first.iter())
        .filter(|item| {
            !blocked.contains(&item.track_ref.key) && seen.insert(item.track_ref.key.clone())
        })
        .map(|item| {
            if item.eligibility == CandidateEligibility::Unknown {
                if let Some(previous) = first.iter().find(|previous| {
                    previous.track_ref.key == item.track_ref.key
                        && previous.eligibility == CandidateEligibility::Eligible
                }) {
                    return previous.clone();
                }
            }
            item.clone()
        })
        .collect()
}

/// AI 点歌的在线检索范围。
///
/// 未获得好友及以上权限的大厅成员只用 QQ、网易、酷狗三源；好友私聊与大厅里
/// 靠身份映射获得好友/管理员/主人权限的成员，按好友私聊的语义处理：
/// `@AI点歌`/`@AI搜索` 搜索全部音源（含 B站），显式指定音源时仍只用该音源。
fn ai_candidate_source(song: &SongCommand, friend_or_above: bool) -> &'static str {
    if !friend_or_above && song.friend_username.trim().is_empty() {
        return "qqmusic,netease,kugou";
    }
    if song.ai_assisted && song.source == SongSource::QqMusic {
        // 大厅命令表的 AI 点歌默认写的是 QQ，获得好友及以上权限后按好友表放开全部音源。
        return "";
    }
    song.source.as_str()
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

    struct RewritingAiGateway {
        query: String,
        fail: bool,
        calls: Mutex<Vec<(String, bool)>>,
        picks: RecordingAiGateway,
    }
    impl SongRequestAiGateway for RewritingAiGateway {
        fn enabled(&self) -> bool {
            true
        }
        fn rewrite_song_search(
            &self,
            request: &str,
            prefer: bool,
            _candidates: &[SearchCandidate],
        ) -> Result<Option<String>> {
            self.calls.lock().unwrap().push((request.into(), prefer));
            if self.fail {
                return Err(anyhow!("mock semantic failure"));
            }
            Ok((!self.query.is_empty()).then(|| self.query.clone()))
        }
        fn pick_song_candidate(
            &self,
            request: &str,
            prefer: bool,
            candidates: &[SearchCandidate],
        ) -> Result<AiCandidatePickResult> {
            self.picks.pick_song_candidate(request, prefer, candidates)
        }
    }
    fn rewriting_ai(query: &str, fail: bool) -> Arc<RewritingAiGateway> {
        Arc::new(RewritingAiGateway {
            query: query.into(),
            fail,
            calls: Mutex::new(Vec::new()),
            picks: RecordingAiGateway::default(),
        })
    }
    fn retry_application(ai: Arc<RewritingAiGateway>) -> SongRequestApplication {
        SongRequestApplication::with_gateways(ai, Arc::new(DisabledReviewGateway), 20, true)
    }

    struct DisabledAiGateway;

    struct IndexedAiGateway {
        index: usize,
    }

    #[derive(Default)]
    struct RecordingAiGateway {
        candidates: Mutex<Vec<Vec<SearchCandidate>>>,
    }

    impl SongRequestAiGateway for RecordingAiGateway {
        fn enabled(&self) -> bool {
            true
        }

        fn pick_song_candidate(
            &self,
            _request: &str,
            _prefer_accompaniment: bool,
            candidates: &[SearchCandidate],
        ) -> Result<AiCandidatePickResult> {
            self.candidates
                .lock()
                .expect("AI candidates")
                .push(candidates.to_vec());
            Ok(AiCandidatePickResult {
                index: 1,
                reason: "test selection".to_string(),
                score: 0.9,
            })
        }
    }

    /// 按调用顺序返回 (候选序号, 分数) 的选歌网关：
    /// 第一次是在线/合并候选打分，后续调用用于单独比较本地或在线候选。
    struct ScoredAiGateway {
        picks: Mutex<Vec<(usize, f64)>>,
        calls: Mutex<Vec<Vec<SearchCandidate>>>,
    }

    impl ScoredAiGateway {
        fn new(picks: Vec<(usize, f64)>) -> Arc<Self> {
            Arc::new(Self {
                picks: Mutex::new(picks),
                calls: Mutex::new(Vec::new()),
            })
        }
    }

    impl SongRequestAiGateway for ScoredAiGateway {
        fn enabled(&self) -> bool {
            true
        }

        fn pick_song_candidate(
            &self,
            _request: &str,
            _prefer_accompaniment: bool,
            candidates: &[SearchCandidate],
        ) -> Result<AiCandidatePickResult> {
            self.calls.lock().unwrap().push(candidates.to_vec());
            let (index, score) = {
                let mut picks = self.picks.lock().unwrap();
                if picks.is_empty() {
                    (1, 0.9)
                } else {
                    picks.remove(0)
                }
            };
            Ok(AiCandidatePickResult {
                index,
                reason: "test selection".to_string(),
                score,
            })
        }
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
            friend_or_above: false,
        }
    }

    /// 大厅里靠身份映射获得好友及以上权限的成员：命令仍是大厅形状，权限来自映射。
    fn mapped_context() -> SongRequestContext {
        SongRequestContext {
            friend_or_above: true,
            ..context()
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
        /// 每次等待记录 (允许换源, 允许AI, 允许本地推荐, 超时确认)。
        decision_options: RefCell<Vec<(bool, bool, bool, bool)>>,
        decisions: VecDeque<SongRequestDecision>,
        searches: RefCell<VecDeque<Option<PickedCandidate>>>,
        search_sources: RefCell<Vec<String>>,
        library_candidates: RefCell<Vec<SearchCandidate>>,
        /// 记录本地曲库检索的参数：关键词与「是否允许 B站 音源」。
        library_searches: RefCell<Vec<(String, bool)>>,
        library_error: Cell<bool>,
        local_recommend_enabled: Cell<bool>,
        /// 在线候选是否被关键词字面命中；默认 true 表示平台结果已经命中关键词。
        online_candidate_matches: Cell<bool>,
        online_error: Cell<bool>,
        fail_search_call: Cell<usize>,
        queue: RefCell<Vec<QueueItem>>,
        status: FakeStatus,
        should_queue: bool,
        status_matches: bool,
        play_outcome: PlaybackOutcome,
        play_final_request: Option<PlaybackRequest>,
        played: RefCell<Vec<ResolvedSongRequest>>,
        preloaded: RefCell<Vec<PlayableTrack>>,
        dedup_limited: Cell<bool>,
        logs: RefCell<Vec<String>>,
    }

    impl FakePort {
        fn idle(searches: impl IntoIterator<Item = Option<PickedCandidate>>) -> Self {
            Self {
                replies: RefCell::new(Vec::new()),
                decision_prompts: RefCell::new(Vec::new()),
                decision_options: RefCell::new(Vec::new()),
                decisions: VecDeque::from([SongRequestDecision::Confirm]),
                searches: RefCell::new(searches.into_iter().collect()),
                search_sources: RefCell::new(Vec::new()),
                library_candidates: RefCell::new(Vec::new()),
                library_searches: RefCell::new(Vec::new()),
                library_error: Cell::new(false),
                local_recommend_enabled: Cell::new(true),
                online_candidate_matches: Cell::new(true),
                online_error: Cell::new(false),
                fail_search_call: Cell::new(0),
                queue: RefCell::new(Vec::new()),
                status: FakeStatus::Available(Box::new(stopped_status())),
                should_queue: false,
                status_matches: false,
                play_outcome: PlaybackOutcome::Success,
                play_final_request: None,
                played: RefCell::new(Vec::new()),
                preloaded: RefCell::new(Vec::new()),
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

        fn prompt_and_wait_for_decision_batch(
            &mut self,
            messages: &[String],
            allow_switch_source: bool,
            allow_ai: bool,
            allow_local: bool,
            default_confirm: bool,
        ) -> Result<SongRequestDecision> {
            self.decision_options.borrow_mut().push((
                allow_switch_source,
                allow_ai,
                default_confirm,
                allow_local,
            ));
            if let Some(first) = messages.first() {
                self.decision_prompts.borrow_mut().push(first.clone());
            }
            self.replies.borrow_mut().extend(messages.iter().cloned());
            // 与运行时一致：没有开放本地推荐时 @本地 被忽略，继续等下一个决定。
            let mut decision = self
                .decisions
                .pop_front()
                .unwrap_or(SongRequestDecision::Timeout);
            while !allow_local && matches!(decision, SongRequestDecision::LocalLibrary) {
                decision = self
                    .decisions
                    .pop_front()
                    .unwrap_or(SongRequestDecision::Timeout);
            }
            Ok(decision)
        }

        fn search_candidates(
            &self,
            _keyword: &str,
            source: &str,
        ) -> std::result::Result<Option<Vec<SearchCandidate>>, SongSearchFailure> {
            self.search_sources.borrow_mut().push(source.to_string());
            if self.online_error.get()
                || self.search_sources.borrow().len() == self.fail_search_call.get()
            {
                return Err(SongSearchFailure::Backend(
                    "test platform unavailable".into(),
                ));
            }
            Ok(self
                .searches
                .borrow_mut()
                .pop_front()
                .flatten()
                .map(|picked| picked.candidate_snapshot))
        }

        fn local_recommend(&self) -> bool {
            self.local_recommend_enabled.get()
        }

        fn search_library_candidates(
            &self,
            keyword: &str,
            allow_bilibili: bool,
        ) -> Result<Vec<SearchCandidate>> {
            self.library_searches
                .borrow_mut()
                .push((keyword.into(), allow_bilibili));
            if self.library_error.get() {
                return Err(anyhow!("test library unavailable"));
            }
            // 与生产实现一致：缺少好友及以上权限时本地兜底排除 B站 音源。
            Ok(self
                .library_candidates
                .borrow()
                .iter()
                .filter(|candidate| {
                    allow_bilibili
                        || candidate.track_ref.key.provider
                            != miliastra_playback::ProviderId::Bilibili
                })
                .cloned()
                .collect())
        }

        fn online_candidate_matches_keyword(
            &self,
            _keyword: &str,
            _candidate: &SearchCandidate,
        ) -> bool {
            self.online_candidate_matches.get()
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

        fn preload_track(&self, track: &PlayableTrack) -> Result<()> {
            self.preloaded.borrow_mut().push(track.clone());
            Ok(())
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
    fn full_queue_replies_before_searching_for_a_song() {
        let mut port = FakePort::idle(std::iter::empty());
        port.queue.borrow_mut().push(QueueItem {
            keyword: "已有歌曲".to_string(),
            ..QueueItem::default()
        });
        let application = SongRequestApplication::with_gateways(
            Arc::new(DisabledAiGateway),
            Arc::new(DisabledReviewGateway),
            1,
            true,
        );

        application
            .execute(&context(), &command(), &mut port)
            .expect("full queue should be handled");

        assert_eq!(port.replies.borrow().as_slice(), ["队列已满，请稍后再试"]);
        assert!(port.search_sources.borrow().is_empty());
        assert!(port.played.borrow().is_empty());
        assert_eq!(port.logs.borrow().as_slice(), ["queue-full-early"]);
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
            queue_item_id: None,
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
    fn selecting_a_candidate_uses_the_requested_track_metadata() {
        let first = test_candidate("第一首", "miliastra://track/qqmusic/1");
        let second = test_candidate("第二首", "miliastra://track/qqmusic/2");
        let mut selected = picked("第一首", "miliastra://track/qqmusic/1");
        selected.candidate_snapshot = vec![first, second];
        let mut port = FakePort::idle([Some(selected)]);
        port.decisions = VecDeque::from([
            SongRequestDecision::Select,
            SongRequestDecision::SelectIndex(2),
        ]);

        application()
            .execute(&context(), &command(), &mut port)
            .expect("song request");

        assert_eq!(
            port.played.borrow()[0]
                .track
                .as_ref()
                .map(|track| track.track_ref.key.to_string())
                .as_deref(),
            Some("miliastra://track/qqmusic/2")
        );
        assert!(
            port.replies
                .borrow()
                .iter()
                .any(|reply| reply.starts_with("@1 "))
        );
        assert!(
            port.replies
                .borrow()
                .iter()
                .any(|reply| reply.starts_with("@2 "))
        );
    }

    #[test]
    fn selecting_source_switches_candidate_search_to_the_other_provider() {
        let mut port = FakePort::idle([
            Some(picked("晴天 - 周杰伦", "miliastra://track/qqmusic/1")),
            Some(picked("晴天 - 周杰伦", "miliastra://track/netease/2")),
        ]);
        port.decisions = VecDeque::from([
            SongRequestDecision::Select,
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
        assert!(
            port.replies
                .borrow()
                .iter()
                .any(|reply| reply.contains("@换源重新搜索"))
        );
    }

    #[test]
    fn invalid_candidate_numbers_and_repeated_selection_keep_waiting() {
        let mut port =
            FakePort::idle([Some(picked("晴天 - 周杰伦", "miliastra://track/qqmusic/1"))]);
        port.decisions = VecDeque::from([
            SongRequestDecision::SelectIndex(5),
            SongRequestDecision::Select,
            SongRequestDecision::SelectIndex(5),
            SongRequestDecision::SelectIndex(1),
        ]);

        application()
            .execute(&context(), &command(), &mut port)
            .expect("selection after invalid input");

        assert_eq!(port.played.borrow().len(), 1);
        assert_eq!(
            port.played.borrow()[0]
                .track
                .as_ref()
                .unwrap()
                .track_ref
                .key
                .id,
            "1"
        );
        assert_eq!(
            port.decision_options.borrow().as_slice(),
            [
                (true, false, true, false),
                (true, false, false, false),
                (true, false, false, false),
                (true, false, false, false),
            ]
        );
    }

    #[test]
    fn candidate_selection_retry_preserves_timeout_cancellation() {
        let mut port =
            FakePort::idle([Some(picked("晴天 - 周杰伦", "miliastra://track/qqmusic/1"))]);
        port.decisions = VecDeque::from([
            SongRequestDecision::Select,
            SongRequestDecision::SelectIndex(5),
            SongRequestDecision::Cancelled,
        ]);

        application()
            .execute(&context(), &command(), &mut port)
            .expect("selection timeout");

        assert!(port.played.borrow().is_empty());
        assert!(port.queue.borrow().is_empty());
        assert_eq!(port.decision_options.borrow().len(), 3);
    }

    #[test]
    fn semantic_retry_searches_twice_with_original_source_and_request() {
        let ai = rewriting_ai("晴天 周杰伦", false);
        let app = retry_application(ai.clone());
        let found = local_candidate("found");
        let mut port = FakePort::idle([
            None,
            Some(PickedCandidate::with_snapshot(
                found.clone(),
                vec![found.clone()],
                "",
            )),
        ]);
        let song = SongCommand {
            keyword: "周董的晴天".into(),
            ai_assisted: true,
            ..command()
        };
        app.execute(&context(), &song, &mut port).unwrap();
        assert_eq!(
            ai.calls.lock().unwrap().as_slice(),
            [(song.keyword.clone(), false)]
        );
        assert_eq!(
            port.library_searches.borrow().as_slice(),
            [
                (song.keyword.clone(), context().friend_or_above),
                ("晴天 周杰伦".into(), context().friend_or_above)
            ]
        );
        assert_eq!(port.search_sources.borrow().len(), 2);
        assert_eq!(
            port.played.borrow()[0]
                .track
                .as_ref()
                .unwrap()
                .track_ref
                .key,
            found.track_ref.key
        );
        assert!(
            port.decision_prompts
                .borrow()
                .iter()
                .any(|text| text.contains("AI匹配"))
        );
    }

    #[test]
    fn semantic_retry_preserves_first_candidates_on_empty_or_failed_second_search() {
        for fail in [false, true] {
            let ai = rewriting_ai("晴天 周杰伦", false);
            let app = retry_application(ai.clone());
            let first = local_candidate("first");
            let mut port = FakePort::idle([
                Some(PickedCandidate::with_snapshot(
                    first.clone(),
                    vec![first.clone()],
                    "",
                )),
                None,
            ]);
            if fail {
                port.fail_search_call.set(2);
            }
            app.execute(
                &context(),
                &SongCommand {
                    keyword: "周董晴天".into(),
                    ai_assisted: true,
                    ..command()
                },
                &mut port,
            )
            .unwrap();
            assert_eq!(port.search_sources.borrow().len(), 2);
            assert_eq!(ai.calls.lock().unwrap().len(), 1);
            assert_eq!(
                port.played.borrow()[0]
                    .track
                    .as_ref()
                    .unwrap()
                    .track_ref
                    .key,
                first.track_ref.key
            );
        }
    }

    #[test]
    fn semantic_retry_skips_duplicate_invalid_or_failed_rewrites() {
        for (query, fail) in [
            ("晴天", false),
            ("", false),
            ("@点歌 晴天", false),
            ("晴天 周杰伦", true),
        ] {
            let ai = rewriting_ai(query, fail);
            let app = retry_application(ai.clone());
            let first = local_candidate("first");
            let mut port = FakePort::idle([Some(PickedCandidate::with_snapshot(
                first.clone(),
                vec![first],
                "",
            ))]);
            app.execute(
                &context(),
                &SongCommand {
                    ai_assisted: true,
                    ..command()
                },
                &mut port,
            )
            .unwrap();
            assert_eq!(port.search_sources.borrow().len(), 1);
            assert_eq!(port.played.borrow().len(), 1);
        }
    }

    #[test]
    fn semantic_retry_empty_results_are_bounded_and_skip_does_not_play() {
        for empty in [false, true] {
            let ai = rewriting_ai("晴天 周杰伦", false);
            let app = retry_application(ai.clone());
            let found = local_candidate("found");
            let result =
                (!empty).then(|| PickedCandidate::with_snapshot(found.clone(), vec![found], ""));
            let mut port = FakePort::idle([None, result]);
            port.decisions = VecDeque::from([SongRequestDecision::Skip]);
            app.execute(
                &context(),
                &SongCommand {
                    keyword: "周董晴天".into(),
                    ai_assisted: true,
                    ..command()
                },
                &mut port,
            )
            .unwrap();
            assert_eq!(port.search_sources.borrow().len(), 2);
            assert_eq!(ai.calls.lock().unwrap().len(), 1);
            assert!(port.played.borrow().is_empty());
            assert!(port.preloaded.borrow().is_empty());
            assert!(port.queue.borrow().is_empty());
        }
    }

    #[test]
    fn semantic_retry_preserves_accompaniment_and_does_not_run_for_ordinary_requests() {
        let ai = rewriting_ai("晴天 周杰伦", false);
        let app = retry_application(ai.clone());
        let mut port = FakePort::idle([None, None]);
        app.execute(
            &context(),
            &SongCommand {
                keyword: "周董晴天".into(),
                ai_assisted: true,
                prefer_accompaniment: true,
                ..command()
            },
            &mut port,
        )
        .unwrap();
        assert_eq!(port.library_searches.borrow()[1].0, "晴天 周杰伦 伴奏");
        let ordinary_ai = rewriting_ai("别的查询", false);
        let found = local_candidate("found");
        let mut ordinary = FakePort::idle([Some(PickedCandidate::with_snapshot(
            found.clone(),
            vec![found],
            "",
        ))]);
        retry_application(ordinary_ai.clone())
            .execute(&context(), &command(), &mut ordinary)
            .unwrap();
        assert!(ordinary_ai.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn semantic_retry_merge_cannot_resurrect_restricted_tracks() {
        assert!(SongRequestDecision::is_feedback_text(
            "AI理解搜索词:晴天 周杰伦，再次搜索"
        ));
        assert!(SongRequestDecision::is_feedback_text(
            "二次搜索失败，保留首次搜索结果"
        ));
        let mut blocked = local_candidate("blocked");
        blocked.eligibility = CandidateEligibility::VipRequired;
        let first = local_candidate("first");
        let second = local_candidate("second");
        let merged = merge_ai_search_rounds(
            vec![blocked, first.clone()],
            vec![local_candidate("blocked"), second.clone(), first],
        );
        assert_eq!(
            merged
                .iter()
                .map(|item| item.track_ref.key.id.as_str())
                .collect::<Vec<_>>(),
            ["second", "first"]
        );
        let ai = rewriting_ai("晴天 周杰伦", false);
        let mut blocked = local_candidate("only");
        blocked.eligibility = CandidateEligibility::NoCopyright;
        let mut port = FakePort::idle([Some(PickedCandidate::with_snapshot(
            blocked.clone(),
            vec![blocked],
            "",
        ))]);
        retry_application(ai.clone())
            .execute(
                &context(),
                &SongCommand {
                    ai_assisted: true,
                    ..command()
                },
                &mut port,
            )
            .unwrap();
        assert!(ai.calls.lock().unwrap().is_empty());
        assert!(port.played.borrow().is_empty());
    }

    fn local_candidate(id: &str) -> SearchCandidate {
        let mut candidate = test_candidate(id, &format!("miliastra://track/qqmusic/{id}"));
        candidate.eligibility = CandidateEligibility::Unknown;
        candidate
    }

    #[test]
    fn ai_library_candidates_are_reviewed_and_selected_when_platform_has_no_match() {
        let ai = Arc::new(RecordingAiGateway::default());
        let review = Arc::new(AllowingReviewGateway {
            candidates: Mutex::new(Vec::new()),
        });
        let application =
            SongRequestApplication::with_gateways(ai.clone(), review.clone(), 20, true);
        let local = local_candidate("local");
        let mut port = FakePort::idle([None]);
        port.library_candidates.borrow_mut().push(local.clone());
        let song = SongCommand {
            ai_assisted: true,
            ..command()
        };
        application.execute(&context(), &song, &mut port).unwrap();
        assert_eq!(
            port.library_searches.borrow().as_slice(),
            [("晴天".into(), context().friend_or_above)]
        );
        assert_eq!(ai.candidates.lock().unwrap()[0].len(), 1);
        assert_eq!(
            port.played.borrow()[0]
                .track
                .as_ref()
                .unwrap()
                .track_ref
                .key,
            local.track_ref.key
        );
        assert_eq!(review.candidates.lock().unwrap().len(), 1);
        assert!(
            port.decision_prompts
                .borrow()
                .iter()
                .any(|prompt| prompt.contains("曲库"))
        );
    }

    #[test]
    fn plain_request_offers_local_library_when_online_pick_is_not_a_literal_match() {
        // 在线平台只给了近似结果（未被关键词字面命中）而本地库有命中的记录：
        // 普通点歌也追加本地推荐，@本地 可用，超时按跳过处理，不会自动播放。
        let online = local_candidate("online");
        let local = local_candidate("library");
        let mut port = FakePort::idle([Some(PickedCandidate::with_snapshot(
            online.clone(),
            vec![online.clone()],
            "",
        ))]);
        port.online_candidate_matches.set(false);
        port.library_candidates.borrow_mut().push(local.clone());
        port.decisions = VecDeque::from([SongRequestDecision::LocalLibrary]);
        application()
            .execute(&context(), &command(), &mut port)
            .expect("song request");
        assert_eq!(
            port.library_searches.borrow().as_slice(),
            [("晴天".to_string(), false)]
        );
        assert!(
            port.replies
                .borrow()
                .iter()
                .any(|line| line.contains("本地曲库推荐"))
        );
        // 记录顺序为 (允许换源, 允许AI, 超时确认, 允许本地推荐)。
        assert!(port.decision_options.borrow()[0].3, "应允许 @本地");
        assert!(
            !port.decision_options.borrow()[0].2,
            "有本地推荐时超时不确认"
        );
        assert_eq!(
            port.played.borrow()[0]
                .track
                .as_ref()
                .unwrap()
                .track_ref
                .key
                .id,
            "library"
        );

        // 用户直接 @确认 时仍然播在线候选。
        let mut confirm = FakePort::idle([Some(PickedCandidate::with_snapshot(
            online.clone(),
            vec![online.clone()],
            "",
        ))]);
        confirm.online_candidate_matches.set(false);
        confirm.library_candidates.borrow_mut().push(local.clone());
        confirm.decisions = VecDeque::from([SongRequestDecision::Confirm]);
        application()
            .execute(&context(), &command(), &mut confirm)
            .expect("song request");
        assert_eq!(
            confirm.played.borrow()[0]
                .track
                .as_ref()
                .unwrap()
                .track_ref
                .key
                .id,
            "online"
        );

        // 超时（运行时映射为 Cancelled）时不播放任何候选。
        let mut timeout = FakePort::idle([Some(PickedCandidate::with_snapshot(
            online.clone(),
            vec![online.clone()],
            "",
        ))]);
        timeout.online_candidate_matches.set(false);
        timeout.library_candidates.borrow_mut().push(local);
        timeout.decisions = VecDeque::from([SongRequestDecision::Cancelled]);
        application()
            .execute(&context(), &command(), &mut timeout)
            .expect("song request");
        assert!(timeout.played.borrow().is_empty());
    }

    #[test]
    fn ai_library_candidates_survive_platform_failure_and_literal_matches_skip_the_library() {
        let ai = Arc::new(RecordingAiGateway::default());
        let application = SongRequestApplication::with_gateways(
            ai.clone(),
            Arc::new(DisabledReviewGateway),
            20,
            true,
        );
        let local = local_candidate("local");
        let mut port = FakePort::idle([None]);
        port.library_candidates.borrow_mut().push(local.clone());
        port.online_error.set(true);
        application
            .execute(
                &context(),
                &SongCommand {
                    ai_assisted: true,
                    ..command()
                },
                &mut port,
            )
            .unwrap();
        assert_eq!(port.played.borrow().len(), 1);
        let online = local_candidate("online");
        let mut normal = FakePort::idle([Some(PickedCandidate::with_snapshot(
            online.clone(),
            vec![online.clone()],
            "",
        ))]);
        normal.library_candidates.borrow_mut().push(local);
        application
            .execute(&context(), &command(), &mut normal)
            .unwrap();
        assert!(normal.library_searches.borrow().is_empty());
        assert_eq!(
            normal.played.borrow()[0]
                .track
                .as_ref()
                .unwrap()
                .track_ref
                .key,
            online.track_ref.key
        );
    }

    #[test]
    fn ai_library_failure_does_not_prevent_online_selection() {
        let ai = Arc::new(RecordingAiGateway::default());
        let application = SongRequestApplication::with_gateways(
            ai.clone(),
            Arc::new(DisabledReviewGateway),
            20,
            true,
        );
        let online = local_candidate("online");
        let mut port = FakePort::idle([Some(PickedCandidate::with_snapshot(
            online.clone(),
            vec![online.clone()],
            "",
        ))]);
        port.library_error.set(true);
        application
            .execute(
                &context(),
                &SongCommand {
                    ai_assisted: true,
                    ..command()
                },
                &mut port,
            )
            .unwrap();
        assert_eq!(
            port.played.borrow()[0]
                .track
                .as_ref()
                .unwrap()
                .track_ref
                .key,
            online.track_ref.key
        );
    }

    #[test]
    fn ai_library_unknown_never_overrides_online_ineligibility() {
        for eligibility in [
            CandidateEligibility::VipRequired,
            CandidateEligibility::PaidRequired,
            CandidateEligibility::NoCopyright,
            CandidateEligibility::Ineligible,
        ] {
            let ai = Arc::new(RecordingAiGateway::default());
            let application = SongRequestApplication::with_gateways(
                ai.clone(),
                Arc::new(DisabledReviewGateway),
                20,
                true,
            );
            let local = local_candidate("same");
            let mut online = local.clone();
            online.eligibility = eligibility;
            let mut port = FakePort::idle([Some(PickedCandidate::with_snapshot(
                online.clone(),
                vec![online],
                "",
            ))]);
            port.library_candidates.borrow_mut().push(local);
            application
                .execute(
                    &context(),
                    &SongCommand {
                        ai_assisted: true,
                        ..command()
                    },
                    &mut port,
                )
                .unwrap();
            assert!(ai.candidates.lock().unwrap().is_empty());
            assert!(port.played.borrow().is_empty());
            assert!(port.queue.borrow().is_empty());
        }
    }

    #[test]
    fn ai_library_merge_preserves_online_metadata_and_unique_candidate_numbers() {
        let local = local_candidate("same");
        let mut fresh = local.clone();
        fresh.metadata.title = "新元数据".into();
        fresh.eligibility = CandidateEligibility::Eligible;
        let other = local_candidate("other");
        let merged = merge_ai_search_candidates(
            vec![local.clone(), local],
            vec![fresh.clone(), other.clone(), other],
        );
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].metadata, fresh.metadata);
        assert_eq!(merged[0].eligibility, CandidateEligibility::Eligible);
        assert!(merged[0].text.contains("曲库"));
    }

    #[test]
    fn ai_library_and_online_manual_selection_use_the_same_candidate_snapshot() {
        let ai = Arc::new(RecordingAiGateway::default());
        let application = SongRequestApplication::with_gateways(
            ai.clone(),
            Arc::new(DisabledReviewGateway),
            20,
            true,
        );
        let online = local_candidate("online");
        let mut port = FakePort::idle([Some(PickedCandidate::with_snapshot(
            online.clone(),
            vec![online.clone()],
            "",
        ))]);
        port.library_candidates
            .borrow_mut()
            .push(local_candidate("local"));
        port.decisions = VecDeque::from([
            SongRequestDecision::Select,
            SongRequestDecision::SelectIndex(2),
        ]);
        application
            .execute(
                &context(),
                &SongCommand {
                    ai_assisted: true,
                    ..command()
                },
                &mut port,
            )
            .unwrap();
        let candidates = ai.candidates.lock().unwrap();
        assert_eq!(candidates[0].len(), 2);
        assert_eq!(port.played.borrow()[0].candidate_snapshot, candidates[0]);
        assert_eq!(
            port.played.borrow()[0]
                .track
                .as_ref()
                .unwrap()
                .track_ref
                .key,
            online.track_ref.key
        );
    }

    #[test]
    fn ai_library_skip_does_not_play_preload_or_queue() {
        let ai = Arc::new(RecordingAiGateway::default());
        let application =
            SongRequestApplication::with_gateways(ai, Arc::new(DisabledReviewGateway), 20, true);
        let mut port = FakePort::idle([None]);
        port.library_candidates
            .borrow_mut()
            .push(local_candidate("local"));
        port.decisions = VecDeque::from([SongRequestDecision::Skip]);
        application
            .execute(
                &context(),
                &SongCommand {
                    ai_assisted: true,
                    ..command()
                },
                &mut port,
            )
            .unwrap();
        assert!(port.played.borrow().is_empty());
        assert!(port.preloaded.borrow().is_empty());
        assert!(port.queue.borrow().is_empty());
    }

    #[test]
    fn ai_library_no_match_and_platform_failure_keep_existing_error_behavior() {
        let ai = Arc::new(RecordingAiGateway::default());
        let application = SongRequestApplication::with_gateways(
            ai.clone(),
            Arc::new(DisabledReviewGateway),
            20,
            true,
        );
        let mut port = FakePort::idle([None]);
        port.online_error.set(true);
        application
            .execute(
                &context(),
                &SongCommand {
                    ai_assisted: true,
                    ..command()
                },
                &mut port,
            )
            .unwrap();
        assert!(ai.candidates.lock().unwrap().is_empty());
        assert!(port.played.borrow().is_empty());
        assert!(
            port.replies
                .borrow()
                .iter()
                .any(|reply| reply.contains("歌曲搜索后端失败"))
        );
    }

    #[test]
    fn ai_and_manual_selection_share_only_playable_or_unknown_candidates() {
        let ai = Arc::new(RecordingAiGateway::default());
        let application = SongRequestApplication::with_gateways(
            ai.clone(),
            Arc::new(DisabledReviewGateway),
            20,
            true,
        );
        let candidates = [
            ("no-copyright", CandidateEligibility::NoCopyright),
            ("eligible", CandidateEligibility::Eligible),
            ("vip-required", CandidateEligibility::VipRequired),
            ("paid-required", CandidateEligibility::PaidRequired),
            ("ineligible", CandidateEligibility::Ineligible),
            ("unknown", CandidateEligibility::Unknown),
        ]
        .into_iter()
        .map(|(id, eligibility)| {
            let mut candidate = test_candidate(id, &format!("miliastra://track/qqmusic/{id}"));
            candidate.eligibility = eligibility;
            candidate
        })
        .collect::<Vec<_>>();
        let expected = vec![candidates[1].clone(), candidates[5].clone()];
        let mut port = FakePort::idle([Some(PickedCandidate::with_snapshot(
            candidates[0].clone(),
            candidates,
            "",
        ))]);
        port.decisions = VecDeque::from([
            SongRequestDecision::Select,
            SongRequestDecision::SelectIndex(2),
        ]);

        application
            .execute(
                &context(),
                &SongCommand {
                    ai_assisted: true,
                    ..command()
                },
                &mut port,
            )
            .expect("AI selection from usable candidates");

        assert_eq!(
            ai.candidates.lock().expect("AI candidates").as_slice(),
            std::slice::from_ref(&expected)
        );
        assert_eq!(port.played.borrow().len(), 1);
        assert_eq!(port.played.borrow()[0].candidate_snapshot, expected);
        assert_eq!(
            port.played.borrow()[0]
                .track
                .as_ref()
                .unwrap()
                .track_ref
                .key
                .id,
            "unknown"
        );
        assert!(
            port.replies
                .borrow()
                .iter()
                .any(|reply| reply.starts_with("@1 eligible"))
        );
        assert!(
            port.replies
                .borrow()
                .iter()
                .any(|reply| reply.starts_with("@2 unknown"))
        );
        assert_eq!(
            port.decision_options.borrow().as_slice(),
            [(false, false, true, false), (false, false, false, false)]
        );
        assert!(
            port.replies
                .borrow()
                .iter()
                .all(|reply| !reply.contains("@换源"))
        );
    }

    #[test]
    fn all_unplayable_ai_candidates_stop_before_model_or_playback_calls() {
        let ai = Arc::new(RecordingAiGateway::default());
        let application = SongRequestApplication::with_gateways(
            ai.clone(),
            Arc::new(DisabledReviewGateway),
            20,
            true,
        );
        let mut candidate = test_candidate("无版权", "miliastra://track/qqmusic/unavailable");
        candidate.eligibility = CandidateEligibility::NoCopyright;
        let mut port = FakePort::idle([Some(PickedCandidate::new(candidate, ""))]);

        application
            .execute(
                &context(),
                &SongCommand {
                    ai_assisted: true,
                    ..command()
                },
                &mut port,
            )
            .expect("no playable AI candidates");

        assert!(ai.candidates.lock().expect("AI candidates").is_empty());
        assert!(port.played.borrow().is_empty());
        assert!(port.queue.borrow().is_empty());
        assert!(port.decision_prompts.borrow().is_empty());
        assert_eq!(
            port.replies.borrow().last().map(String::as_str),
            Some("平台无对应歌曲音源")
        );
    }

    #[test]
    fn song_review_uses_structured_metadata_for_regular_and_ai_requests() {
        for (ai_assisted, source, source_label) in
            [(false, "qqmusic", "QQ"), (true, "netease", "网易")]
        {
            let review = Arc::new(AllowingReviewGateway {
                candidates: Mutex::new(Vec::new()),
            });
            let application = SongRequestApplication::with_gateways(
                Arc::new(IndexedAiGateway { index: 1 }),
                review.clone(),
                20,
                true,
            );
            let mut candidate = test_candidate(
                &format!("晴天 - Jay - Guest / 周杰伦 [{source_label} 03:29]"),
                &format!("miliastra://track/{source}/review"),
            );
            candidate.metadata.title = "晴天".to_string();
            candidate.metadata.artists = vec!["Jay - Guest".to_string(), "周杰伦".to_string()];
            candidate.metadata.duration_ms = Some(209_000);
            let mut port = FakePort::idle([Some(PickedCandidate::new(candidate, ""))]);

            application
                .execute(
                    &context(),
                    &SongCommand {
                        ai_assisted,
                        ..command()
                    },
                    &mut port,
                )
                .expect("reviewed song request");

            let candidates = review.candidates.lock().expect("review candidates");
            assert_eq!(candidates.len(), 1);
            assert_eq!(candidates[0].title, "晴天");
            assert_eq!(candidates[0].artist, "Jay - Guest / 周杰伦");
            assert_eq!(candidates[0].source, source);
            assert_eq!(candidates[0].duration_ms, Some(209_000));
            assert_eq!(port.played.borrow().len(), 1);
        }
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
        assert_eq!(port.preloaded.borrow().len(), 1);
        assert_eq!(
            port.preloaded.borrow()[0].track_ref.key.to_string(),
            "miliastra://track/qqmusic/1"
        );
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
    fn decision_parser_accepts_selection_commands() {
        assert_eq!(
            SongRequestDecision::parse("@选择"),
            Some(SongRequestDecision::Select)
        );
        assert_eq!(
            SongRequestDecision::parse("@换源"),
            Some(SongRequestDecision::SwitchSource)
        );
        assert_eq!(
            SongRequestDecision::parse("@5"),
            Some(SongRequestDecision::SelectIndex(5))
        );
        assert_eq!(SongRequestDecision::parse("@6"), None);
        assert_eq!(SongRequestDecision::parse("@0"), None);
        assert_eq!(
            SongRequestDecision::parse("@选择"),
            Some(SongRequestDecision::Select)
        );
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

    #[test]
    fn hall_ai_song_search_excludes_bilibili_while_friend_ai_search_keeps_it() {
        // 大厅里未映射的成员：不提供 B站 音源。
        let hall = SongCommand {
            friend_username: String::new(),
            ..command()
        };
        let source = ai_candidate_source(&hall, false);
        assert!(!source.split(',').any(|part| part == "bilibili"));
        assert_eq!(source, "qqmusic,netease,kugou");

        // 大厅里靠身份映射获得好友及以上权限的成员：AI 点歌按好友语义搜索全部音源。
        let mapped = SongCommand {
            ai_assisted: true,
            friend_username: String::new(),
            ..command()
        };
        assert_eq!(ai_candidate_source(&mapped, true), "");

        // 好友 AI 点歌：按好友命令的 source（All 为空串 → 全平台，含 B站）。
        let friend_ai = SongCommand {
            source: SongSource::All,
            friend_username: "Alice".to_string(),
            ..command()
        };
        assert_eq!(ai_candidate_source(&friend_ai, true), "");

        // 好友 B站点歌：明确只搜 B站。
        let friend_bilibili = SongCommand {
            source: SongSource::Bilibili,
            friend_username: "Alice".to_string(),
            ..command()
        };
        assert_eq!(ai_candidate_source(&friend_bilibili, true), "bilibili");

        // 显式指定音源的 AI 点歌在映射成员手里也只用该音源。
        let mapped_netease = SongCommand {
            source: SongSource::Netease,
            ai_assisted: true,
            friend_username: String::new(),
            ..command()
        };
        assert_eq!(ai_candidate_source(&mapped_netease, true), "netease");
    }

    #[test]
    fn mapped_hall_member_ai_search_uses_every_platform() {
        // 权限来自身份映射时，AI 点歌的在线检索范围与好友私聊一致（空串=全部音源）。
        let application = SongRequestApplication::with_gateways(
            Arc::new(RecordingAiGateway::default()),
            Arc::new(DisabledReviewGateway),
            20,
            true,
        );
        let song = SongCommand {
            ai_assisted: true,
            ..command()
        };
        let mut mapped = FakePort::idle([None]);
        application
            .execute(&mapped_context(), &song, &mut mapped)
            .expect("song request");
        assert_eq!(mapped.search_sources.borrow().as_slice(), [""]);

        // 大厅里未映射的成员仍然只搜 QQ、网易、酷狗。
        let mut plain = FakePort::idle([None]);
        application
            .execute(&context(), &song, &mut plain)
            .expect("song request");
        assert_eq!(
            plain.search_sources.borrow().as_slice(),
            ["qqmusic,netease,kugou"]
        );
    }

    #[test]
    fn local_library_choices_follow_the_online_sample_count() {
        let local: Vec<_> = (0..6)
            .map(|index| local_candidate(&format!("local{index}")))
            .collect();
        // 在线样本足够多时本地只入 1 个；在线不足时补足到 5 个。
        assert_eq!(local_library_choices(&local, 5).len(), 1);
        assert_eq!(local_library_choices(&local, 0).len(), 5);
        assert_eq!(local_library_choices(&local, 3).len(), 2);
        let online: Vec<_> = (0..6)
            .map(|index| {
                test_candidate(
                    &format!("online{index}"),
                    &format!("miliastra://track/qqmusic/online{index}"),
                )
            })
            .collect();
        let selection = selection_candidates(&online, &local);
        assert_eq!(selection.len(), 5);
        assert_eq!(selection[0].metadata.title, "local0");
        assert!(selection[0].text.contains("曲库"));
        assert_eq!(selection[1].metadata.title, "online0");
        assert_eq!(selection[4].metadata.title, "online3");
    }

    #[test]
    fn ai_low_score_offers_local_library_when_local_score_is_higher() {
        // 第一次在合并候选里选中在线候选(序号 2)、分数 0.3；第二次给本地候选打分 0.85。
        let ai = ScoredAiGateway::new(vec![(2, 0.3), (1, 0.85)]);
        let application = SongRequestApplication::with_gateways(
            ai.clone(),
            Arc::new(DisabledReviewGateway),
            20,
            true,
        );
        let online = local_candidate("online");
        let mut port = FakePort::idle([Some(PickedCandidate::with_snapshot(
            online.clone(),
            vec![online.clone()],
            "",
        ))]);
        port.library_candidates
            .borrow_mut()
            .push(local_candidate("local"));
        port.decisions = VecDeque::from([SongRequestDecision::LocalLibrary]);
        application
            .execute(
                &context(),
                &SongCommand {
                    ai_assisted: true,
                    ..command()
                },
                &mut port,
            )
            .unwrap();
        // 首批消息不重复列出 @本地，入口只在本地推荐行里出现。
        assert!(!port.decision_prompts.borrow()[0].contains("@本地"));
        assert!(
            port.replies.borrow().iter().any(
                |line| line.contains("在线没找到合适的,本地曲库推荐") && line.contains("@本地")
            )
        );
        // 出现本地推荐时超时按跳过处理，不自动确认；本次等待开放 @本地。
        assert!(!port.decision_options.borrow()[0].2);
        assert!(port.decision_options.borrow()[0].3);
        // 本地只有一首，@本地 直接选用并播放。
        assert_eq!(
            port.played.borrow()[0]
                .track
                .as_ref()
                .unwrap()
                .track_ref
                .key
                .id,
            "local"
        );
        // 第二次打分只针对本地候选。
        let calls = ai.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].len(), 1);
        assert_eq!(calls[1][0].metadata.title, "local");
    }

    #[test]
    fn ai_low_score_skips_local_library_when_local_score_is_not_higher() {
        let ai = ScoredAiGateway::new(vec![(2, 0.4), (1, 0.35)]);
        let application = SongRequestApplication::with_gateways(
            ai.clone(),
            Arc::new(DisabledReviewGateway),
            20,
            true,
        );
        let online = local_candidate("online");
        let mut port = FakePort::idle([Some(PickedCandidate::with_snapshot(
            online.clone(),
            vec![online.clone()],
            "",
        ))]);
        port.library_candidates
            .borrow_mut()
            .push(local_candidate("local"));
        application
            .execute(
                &context(),
                &SongCommand {
                    ai_assisted: true,
                    ..command()
                },
                &mut port,
            )
            .unwrap();
        assert!(!port.decision_prompts.borrow()[0].contains("@本地"));
        assert!(
            !port
                .replies
                .borrow()
                .iter()
                .any(|line| line.contains("本地曲库推荐"))
        );
        // 没有本地推荐时保持原有超时确认语义。
        assert!(port.decision_options.borrow()[0].2);
        assert_eq!(
            port.played.borrow()[0]
                .track
                .as_ref()
                .unwrap()
                .track_ref
                .key
                .id,
            "online"
        );
    }

    #[test]
    fn ai_confident_online_match_does_not_score_the_local_library() {
        let ai = ScoredAiGateway::new(vec![(2, 0.95)]);
        let application = SongRequestApplication::with_gateways(
            ai.clone(),
            Arc::new(DisabledReviewGateway),
            20,
            true,
        );
        let online = local_candidate("online");
        let mut port = FakePort::idle([Some(PickedCandidate::with_snapshot(
            online.clone(),
            vec![online.clone()],
            "",
        ))]);
        port.library_candidates
            .borrow_mut()
            .push(local_candidate("local"));
        application
            .execute(
                &context(),
                &SongCommand {
                    ai_assisted: true,
                    ..command()
                },
                &mut port,
            )
            .unwrap();
        assert_eq!(ai.calls.lock().unwrap().len(), 1);
        assert!(!port.decision_prompts.borrow()[0].contains("@本地"));
    }

    #[test]
    fn local_library_timeout_never_plays_a_library_track() {
        // 两首本地候选时，合并列表是 [本地A, 本地B, 在线]，因此在线候选序号是 3。
        let ai = ScoredAiGateway::new(vec![(3, 0.3), (1, 0.9)]);
        let application =
            SongRequestApplication::with_gateways(ai, Arc::new(DisabledReviewGateway), 20, true);
        let online = local_candidate("online");
        let mut port = FakePort::idle([Some(PickedCandidate::with_snapshot(
            online.clone(),
            vec![online.clone()],
            "",
        ))]);
        {
            let mut library = port.library_candidates.borrow_mut();
            library.push(local_candidate("local-a"));
            library.push(local_candidate("local-b"));
        }
        // @本地 之后不再响应：超时必须按跳过处理，不播放任何歌曲。
        port.decisions = VecDeque::from([
            SongRequestDecision::LocalLibrary,
            SongRequestDecision::Cancelled,
        ]);
        application
            .execute(
                &context(),
                &SongCommand {
                    ai_assisted: true,
                    ..command()
                },
                &mut port,
            )
            .unwrap();
        assert!(port.played.borrow().is_empty());
        assert!(
            port.replies
                .borrow()
                .iter()
                .any(|line| line.contains("选择本地曲库歌曲"))
        );
        assert_eq!(port.decision_options.borrow()[1].2, false);
    }

    #[test]
    fn plain_request_without_online_candidate_offers_the_local_library() {
        let mut port = FakePort::idle([None]);
        port.library_candidates
            .borrow_mut()
            .push(local_candidate("local"));
        port.decisions = VecDeque::from([SongRequestDecision::LocalLibrary]);
        application()
            .execute(&context(), &command(), &mut port)
            .expect("song request");
        // 平台无音源时不重复列出 @本地，入口只在本地推荐行里出现。
        assert!(!port.decision_prompts.borrow()[0].contains("@本地"));
        assert!(
            port.replies.borrow().iter().any(
                |line| line.contains("在线没找到合适的,本地曲库推荐") && line.contains("@本地")
            )
        );
        assert!(port.decision_options.borrow()[0].3);
        assert_eq!(
            port.played.borrow()[0]
                .track
                .as_ref()
                .unwrap()
                .track_ref
                .key
                .id,
            "local"
        );
    }
    #[test]
    fn local_library_fallback_is_cross_platform_for_the_hall() {
        // 在线只搜请求的平台（默认 QQ），本地兜底跨平台：库里只有酷狗候选也要能推荐并播放。
        let mut port = FakePort::idle([None]);
        port.library_candidates.borrow_mut().push(test_candidate(
            "降生 纯享版 - 创元-yin",
            "miliastra://track/kugou/78642C20AC3F604F268797FD568A2AF0",
        ));
        port.decisions = VecDeque::from([SongRequestDecision::LocalLibrary]);
        application()
            .execute(&context(), &command(), &mut port)
            .expect("song request");
        assert_eq!(
            port.library_searches.borrow().as_slice(),
            [("晴天".to_string(), false)]
        );
        let played = port.played.borrow();
        assert_eq!(
            played[0].track.as_ref().unwrap().track_ref.key.provider,
            miliastra_playback::ProviderId::Kugou
        );
    }

    fn bilibili_candidate() -> SearchCandidate {
        test_candidate(
            "【小花仙第二季】插曲《降生》纯享版 - 创元-yin",
            "miliastra://track/bilibili/BV1qqpGe1Egj",
        )
    }

    #[test]
    fn local_library_fallback_needs_friend_permission_for_bilibili() {
        // 大厅里未映射的成员没有好友及以上权限：本地兜底排除 B站，@本地 不可用，也不会播放。
        let mut hall = FakePort::idle([None]);
        hall.library_candidates
            .borrow_mut()
            .push(bilibili_candidate());
        application()
            .execute(&context(), &command(), &mut hall)
            .expect("song request");
        assert_eq!(
            hall.library_searches.borrow().as_slice(),
            [("晴天".to_string(), false)]
        );
        assert!(!hall.decision_options.borrow()[0].3);
        assert!(hall.played.borrow().is_empty());
        assert!(
            !hall
                .replies
                .borrow()
                .iter()
                .any(|line| line.contains("@本地"))
        );

        // 大厅里靠身份映射获得好友及以上权限的成员：命令形状仍是大厅命令，
        // 但本地兜底保留 B站 并可以播放。
        let mut mapped = FakePort::idle([None]);
        mapped
            .library_candidates
            .borrow_mut()
            .push(bilibili_candidate());
        mapped.decisions = VecDeque::from([SongRequestDecision::LocalLibrary]);
        application()
            .execute(&mapped_context(), &command(), &mut mapped)
            .expect("song request");
        assert_eq!(
            mapped.library_searches.borrow().as_slice(),
            [("晴天".to_string(), true)]
        );
        let played = mapped.played.borrow();
        assert_eq!(
            played[0].track.as_ref().unwrap().track_ref.key.provider,
            miliastra_playback::ProviderId::Bilibili
        );

        // 好友私聊命令保留 B站 音源，可推荐并播放。
        let mut friend = FakePort::idle([None]);
        friend
            .library_candidates
            .borrow_mut()
            .push(bilibili_candidate());
        friend.decisions = VecDeque::from([SongRequestDecision::LocalLibrary]);
        application()
            .execute(
                &mapped_context(),
                &SongCommand {
                    friend_username: "Bob".to_string(),
                    ..command()
                },
                &mut friend,
            )
            .expect("song request");
        assert_eq!(
            friend.library_searches.borrow().as_slice(),
            [("晴天".to_string(), true)]
        );
    }

    #[test]
    fn local_command_without_recommendation_is_ignored() {
        // 本地分数不高于在线 → 没有本地推荐，此时 @本地 应被忽略而不是终止点歌。
        let ai = ScoredAiGateway::new(vec![(2, 0.4), (1, 0.35)]);
        let application =
            SongRequestApplication::with_gateways(ai, Arc::new(DisabledReviewGateway), 20, true);
        let online = local_candidate("online");
        let mut port = FakePort::idle([Some(PickedCandidate::with_snapshot(
            online.clone(),
            vec![online.clone()],
            "",
        ))]);
        port.library_candidates
            .borrow_mut()
            .push(local_candidate("local"));
        port.decisions = VecDeque::from([
            SongRequestDecision::LocalLibrary,
            SongRequestDecision::Confirm,
        ]);
        application
            .execute(
                &context(),
                &SongCommand {
                    ai_assisted: true,
                    ..command()
                },
                &mut port,
            )
            .unwrap();
        assert!(!port.decision_options.borrow()[0].3);
        assert_eq!(
            port.played.borrow()[0]
                .track
                .as_ref()
                .unwrap()
                .track_ref
                .key
                .id,
            "online"
        );
    }

    #[test]
    fn local_command_is_ignored_when_the_library_has_no_candidate() {
        let mut port = FakePort::idle([None]);
        port.decisions =
            VecDeque::from([SongRequestDecision::LocalLibrary, SongRequestDecision::Skip]);
        application()
            .execute(&context(), &command(), &mut port)
            .expect("song request");
        assert!(!port.decision_options.borrow()[0].3);
        assert!(port.played.borrow().is_empty());
        assert!(
            !port
                .replies
                .borrow()
                .iter()
                .any(|line| line.contains("本地曲库"))
        );
    }

    #[test]
    fn disabled_local_recommend_never_mentions_the_library() {
        let mut port = FakePort::idle([None]);
        port.local_recommend_enabled.set(false);
        port.library_candidates
            .borrow_mut()
            .push(local_candidate("local"));
        port.decisions = VecDeque::from([SongRequestDecision::Skip]);
        application()
            .execute(&context(), &command(), &mut port)
            .expect("song request");
        assert!(!port.decision_prompts.borrow()[0].contains("@本地"));
        assert!(
            !port
                .replies
                .borrow()
                .iter()
                .any(|line| line.contains("本地曲库推荐"))
        );
        assert!(port.played.borrow().is_empty());
    }
}
