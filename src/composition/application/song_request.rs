use super::*;

impl ApplicationRuntime {
    pub(super) fn report_player_search_failure(
        &self,
        label: &str,
        context: &str,
        error: &PlayerSearchClientError,
    ) -> Result<()> {
        log::error!("{context}: {error}");
        self.reply(&format!("{}{}", label, player_search_failure_reply(error)))
    }

    fn resolve_song_request(&mut self, song: &SongCommand) -> Result<Option<ResolvedSongRequest>> {
        if !song.ai_assisted {
            return Ok(Some(ResolvedSongRequest {
                keyword: song.keyword.clone(),
                source: song.source.as_str().to_string(),
                prefer_accompaniment: song.prefer_accompaniment,
                ai_original_text: String::new(),
                uri: String::new(),
                friend_username: song.friend_username.clone(),
                console_bypass_dedup: false,
            }));
        }
        let label = song_label(song);
        if !self.ai.enabled() {
            self.reply(&format!("{}AI点歌未启用，请先配置 ai.api_key", label))?;
            return Ok(None);
        }

        self.reply(&format!("{}AI匹配中", label))?;

        let search_source = ai_candidate_source(song);
        let candidates = match classify_player_search(
            self.player_search
                .search_candidates(&song.keyword, search_source)
                .map(|candidates| (!candidates.is_empty()).then_some(candidates)),
        ) {
            PlayerSearchResolution::Found(candidates) => candidates,
            PlayerSearchResolution::NoSource => {
                self.reply(&format!("{}平台无对应歌曲音源", label))?;
                return Ok(None);
            }
            PlayerSearchResolution::Failed(error) => {
                self.report_player_search_failure(&label, "AI点歌搜索候选失败", &error)?;
                return Ok(None);
            }
        };

        let pick =
            match self
                .ai
                .pick_song_candidate(&song.keyword, song.prefer_accompaniment, &candidates)
            {
                Ok(pick) => pick,
                Err(error) => {
                    log::error!("AI点歌选择候选失败: {error:#}");
                    self.reply(&format!("{}AI点歌识别失败", label))?;
                    return Ok(None);
                }
            };
        let Some(candidate) = candidates
            .iter()
            .find(|candidate| candidate.uri == pick.uri)
        else {
            log::error!("AI点歌返回未知候选: {}", pick.uri);
            self.reply(&format!("{}AI点歌识别失败", label))?;
            return Ok(None);
        };
        log::info!(
            "AI点歌候选: raw={} pick={} uri={} score={:.2} reason={}",
            song.keyword,
            candidate.text,
            candidate.uri,
            pick.score,
            pick.reason
        );
        let message = format!("{}AI匹配:{},@确认@跳过", label, candidate.text);
        self.reply(&message)?;
        match self.wait_for_decision(false, false, true)? {
            UserDecision::Confirm | UserDecision::Timeout => {}
            UserDecision::Skip => return Ok(None),
            UserDecision::Stopped => return Ok(None),
            _ => return Ok(None),
        }
        Ok(Some(ResolvedSongRequest {
            keyword: candidate.text.clone(),
            source: String::new(),
            prefer_accompaniment: song.prefer_accompaniment,
            ai_original_text: song.keyword.clone(),
            uri: candidate.uri.clone(),
            friend_username: song.friend_username.clone(),
            console_bypass_dedup: false,
        }))
    }

    pub(super) fn resolve_and_confirm_song(
        &mut self,
        song: &SongCommand,
    ) -> Result<Option<ResolvedSongRequest>> {
        let Some(request) = self.resolve_song_request(song)? else {
            return Ok(None);
        };
        if request.uri.is_empty() {
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
                PlayerSearchResolution::Found(picked) => Some(picked),
                PlayerSearchResolution::NoSource => None,
                PlayerSearchResolution::Failed(error) => {
                    self.report_player_search_failure(
                        &request_label(&request),
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
                self.reply(&format!(
                    "{}平台无对应歌曲音源,{}",
                    request_label(&request),
                    actions
                ))?;
                let decision = self.wait_for_decision(true, self.ai.enabled(), true)?;
                match decision {
                    UserDecision::SwitchSource => {
                        let next_source = if source == "netease" {
                            "qqmusic"
                        } else {
                            "netease"
                        };
                        return self.resolve_and_confirm_song_with_source(song, next_source);
                    }
                    UserDecision::Ai if self.ai.enabled() => {
                        return self.resolve_and_confirm_song_ai(song);
                    }
                    _ => return Ok(None),
                }
            };
            let song_title = picked.candidate.text.clone();
            let uri = picked.candidate.uri.clone();
            let actions = if self.ai.enabled() {
                "@确认@跳过@换源@AI"
            } else {
                "@确认@跳过@换源"
            };
            self.reply(&format!(
                "{}搜索到:{},{}",
                request_label(&request),
                song_title,
                actions
            ))?;
            let decision = self.wait_for_decision(true, self.ai.enabled(), true)?;
            match decision {
                UserDecision::Confirm | UserDecision::Timeout => {
                    return Ok(Some(ResolvedSongRequest {
                        keyword: picked.candidate.text.clone(),
                        source: source.to_string(),
                        prefer_accompaniment: request.prefer_accompaniment,
                        ai_original_text: String::new(),
                        uri,
                        friend_username: request.friend_username.clone(),
                        console_bypass_dedup: request.console_bypass_dedup,
                    }));
                }
                UserDecision::Skip => {
                    return Ok(None);
                }
                UserDecision::SwitchSource => {
                    let next_source = if source == "netease" {
                        "qqmusic"
                    } else {
                        "netease"
                    };
                    return self.resolve_and_confirm_song_with_source(song, next_source);
                }
                UserDecision::Ai if self.ai.enabled() => {
                    return self.resolve_and_confirm_song_ai(song);
                }
                _ => return Ok(None),
            }
        }
        Ok(Some(request))
    }

    fn resolve_and_confirm_song_with_source(
        &mut self,
        song: &SongCommand,
        source: &str,
    ) -> Result<Option<ResolvedSongRequest>> {
        let picked = match classify_player_search(self.player_search.search_and_pick(
            &song.keyword,
            source,
            song.prefer_accompaniment,
        )) {
            PlayerSearchResolution::Found(picked) => Some(picked),
            PlayerSearchResolution::NoSource => None,
            PlayerSearchResolution::Failed(error) => {
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
            self.reply(&format!("{}换源后仍无音源,{}", song_label(song), actions))?;
            let decision = self.wait_for_decision(true, self.ai.enabled(), true)?;
            match decision {
                UserDecision::SwitchSource => {
                    let next_source = if source == "netease" {
                        "qqmusic"
                    } else {
                        "netease"
                    };
                    return self.resolve_and_confirm_song_with_source(song, next_source);
                }
                UserDecision::Ai if self.ai.enabled() => {
                    return self.resolve_and_confirm_song_ai(song);
                }
                _ => return Ok(None),
            }
        };
        let actions = if self.ai.enabled() {
            "@确认@跳过@换源@AI"
        } else {
            "@确认@跳过@换源"
        };
        self.reply(&format!(
            "{}搜索到:{},{}",
            song_label(song),
            picked.candidate.text,
            actions
        ))?;
        let decision = self.wait_for_decision(true, self.ai.enabled(), true)?;
        match decision {
            UserDecision::Confirm | UserDecision::Timeout => Ok(Some(ResolvedSongRequest {
                keyword: picked.candidate.text.clone(),
                source: source.to_string(),
                prefer_accompaniment: song.prefer_accompaniment,
                ai_original_text: String::new(),
                uri: picked.candidate.uri.clone(),
                friend_username: song.friend_username.clone(),
                console_bypass_dedup: false,
            })),
            UserDecision::Skip => Ok(None),
            UserDecision::SwitchSource => {
                let next_source = if source == "netease" {
                    "qqmusic"
                } else {
                    "netease"
                };
                self.resolve_and_confirm_song_with_source(song, next_source)
            }
            UserDecision::Ai if self.ai.enabled() => self.resolve_and_confirm_song_ai(song),
            _ => Ok(None),
        }
    }

    fn resolve_and_confirm_song_ai(
        &mut self,
        song: &SongCommand,
    ) -> Result<Option<ResolvedSongRequest>> {
        let label = song_label(song);
        if !self.ai.enabled() {
            self.reply(&format!("{}AI点歌未启用", label))?;
            return Ok(None);
        }
        self.reply(&format!("{}AI匹配中", label))?;
        let search_source = ai_candidate_source(song);
        let candidates = match classify_player_search(
            self.player_search
                .search_candidates(&song.keyword, search_source)
                .map(|candidates| (!candidates.is_empty()).then_some(candidates)),
        ) {
            PlayerSearchResolution::Found(candidates) => candidates,
            PlayerSearchResolution::NoSource => {
                self.reply(&format!("{}平台无对应歌曲音源", label))?;
                return Ok(None);
            }
            PlayerSearchResolution::Failed(error) => {
                self.report_player_search_failure(&label, "AI点歌搜索候选失败", &error)?;
                return Ok(None);
            }
        };
        let pick =
            match self
                .ai
                .pick_song_candidate(&song.keyword, song.prefer_accompaniment, &candidates)
            {
                Ok(pick) => pick,
                Err(error) => {
                    log::error!("AI点歌选择候选失败: {error:#}");
                    self.reply(&format!("{}AI点歌识别失败", label))?;
                    return Ok(None);
                }
            };
        let Some(candidate) = candidates.iter().find(|c| c.uri == pick.uri) else {
            log::error!("AI点歌返回未知候选: {}", pick.uri);
            self.reply(&format!("{}AI点歌识别失败", label))?;
            return Ok(None);
        };
        log::info!(
            "AI点歌候选: raw={} pick={} uri={} score={:.2} reason={}",
            song.keyword,
            candidate.text,
            candidate.uri,
            pick.score,
            pick.reason
        );
        let message = format!("{}AI匹配:{},@确认@跳过", label, candidate.text);
        self.reply(&message)?;
        match self.wait_for_decision(false, false, true)? {
            UserDecision::Confirm | UserDecision::Timeout => {}
            UserDecision::Skip => return Ok(None),
            UserDecision::Stopped => return Ok(None),
            _ => return Ok(None),
        }
        Ok(Some(ResolvedSongRequest {
            keyword: candidate.text.clone(),
            source: String::new(),
            prefer_accompaniment: song.prefer_accompaniment,
            ai_original_text: song.keyword.clone(),
            uri: candidate.uri.clone(),
            friend_username: song.friend_username.clone(),
            console_bypass_dedup: false,
        }))
    }

    pub(super) fn queue_contains_request(&self, request: &ResolvedSongRequest) -> Result<bool> {
        self.business
            .playback_queue_contains(QueueItem {
                keyword: request.keyword.clone(),
                source: request.source.clone(),
                prefer_accompaniment: request.prefer_accompaniment,
                uri: request.uri.clone(),
                ..QueueItem::default()
            })
            .map_err(anyhow::Error::from)
    }

    pub(super) fn push_queue_request(
        &self,
        request: &ResolvedSongRequest,
    ) -> Result<QueuePushOutcome> {
        if self.song_dedup_limited(request)? {
            log::info!(
                "长时间同歌去重入队拦截: keyword={} uri={}",
                request.keyword,
                request.uri
            );
            return Ok(QueuePushOutcome::DedupLimited);
        }
        let pushed = self.business.push_playback_queue(QueueItem {
            id: 0,
            keyword: request.keyword.clone(),
            source: request.source.clone(),
            prefer_accompaniment: request.prefer_accompaniment,
            ai_original_text: request.ai_original_text.clone(),
            uri: request.uri.clone(),
            friend_username: request.friend_username.clone(),
            dedup_bypass: request.console_bypass_dedup,
        })?;
        if pushed.accepted {
            Ok(QueuePushOutcome::Added(pushed.size))
        } else {
            Ok(QueuePushOutcome::Full)
        }
    }

    pub(super) fn handle_queue_push_outcome(
        &self,
        parsed: &ParsedCommand,
        request: &ResolvedSongRequest,
        outcome: QueuePushOutcome,
        feedback: QueuePushFeedback,
    ) -> Result<()> {
        match outcome {
            QueuePushOutcome::Added(len) => {
                self.log_executed_command(
                    parsed,
                    &final_song_command_text(request, feedback.queued_action),
                )?;
                self.reply(&format!(
                    "{}({}/{}): {}",
                    feedback.queued_prefix, len, self.config.queue.max_size, request.keyword
                ))?;
            }
            QueuePushOutcome::Full => {
                self.log_executed_command(
                    parsed,
                    &final_song_command_text(request, feedback.full_action),
                )?;
                self.reply(feedback.full_reply)?;
            }
            QueuePushOutcome::DedupLimited => {
                self.log_executed_command(
                    parsed,
                    &final_song_command_text(request, "dedup-limited-queue"),
                )?;
                self.reply(&self.song_dedup_reject_message(request))?;
            }
        }
        Ok(())
    }

    pub(super) fn log_play_request_outcome(
        &self,
        parsed: &ParsedCommand,
        request: &ResolvedSongRequest,
        outcome: PlaybackOutcome,
    ) -> Result<()> {
        let action = match outcome {
            PlaybackOutcome::Success => "play",
            PlaybackOutcome::NoSource => "no-source",
            PlaybackOutcome::Error => "play-error",
            PlaybackOutcome::DedupLimited => "dedup-limited",
        };
        self.log_executed_command(parsed, &final_song_command_text(request, action))
    }

    pub(super) fn song_dedup_limited(&self, request: &ResolvedSongRequest) -> Result<bool> {
        if request.console_bypass_dedup && self.config.song_dedup.console_bypass {
            return Ok(false);
        }
        self.player
            .song_dedup_limited(&self.playback_request_from_resolved(request))
    }

    pub(super) fn song_dedup_reject_message(&self, request: &ResolvedSongRequest) -> String {
        format!("{}近期已播放过,请稍后再点", request.keyword)
    }

    pub(super) fn song_dedup_skip_message(&self, request: &ResolvedSongRequest) -> String {
        format!("{}近期已播放过,已跳过", request.keyword)
    }

    pub(super) fn playback_request_from_resolved(
        &self,
        request: &ResolvedSongRequest,
    ) -> PlaybackRequest {
        PlaybackRequest {
            keyword: request.keyword.clone(),
            source: request.source.clone(),
            prefer_accompaniment: request.prefer_accompaniment,
            uri: request.uri.clone(),
        }
    }

    pub(super) fn review_song_candidate(
        &self,
        parsed: &ParsedCommand,
        request: &ResolvedSongRequest,
    ) -> Result<bool> {
        if !self.song_review.enabled() {
            return Ok(true);
        }
        if parsed.message_type == "控制台" {
            log::info!(
                "候选歌曲审核跳过: 控制台最高权限免审 command={} uri={}",
                parsed.raw,
                request.uri
            );
            return Ok(true);
        }

        let (title, artist) = split_candidate_title_artist(&request.keyword);
        let candidate = SongReviewCandidate {
            source: request.source.clone(),
            title,
            artist,
            uri: request.uri.clone(),
            message_type: parsed.message_type.clone(),
            username: command_username(parsed).to_string(),
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
                    parsed.raw,
                    candidate.title,
                    candidate.artist,
                    candidate.source,
                    candidate.uri,
                    reason
                );
            } else {
                log::info!(
                    "候选歌曲审核通过: level={} threshold={} attempts={} command={} title={} artist={} source={} uri={} reason={} tags={}",
                    level,
                    decision.threshold,
                    decision.attempts,
                    parsed.raw,
                    candidate.title,
                    candidate.artist,
                    candidate.source,
                    candidate.uri,
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
            parsed.raw,
            candidate.title,
            candidate.artist,
            candidate.source,
            candidate.uri,
            reason,
            tags
        );
        let action = decision.level.map_or_else(
            || "review-reject-failed".to_string(),
            |level| format!("review-reject-level-{level}"),
        );
        self.log_executed_command(parsed, &final_song_command_text(request, &action))?;
        self.reply(&review_reject_reply(
            &reason,
            self.song_review.reply_reason_max_chars(),
        ))?;
        Ok(false)
    }
}
