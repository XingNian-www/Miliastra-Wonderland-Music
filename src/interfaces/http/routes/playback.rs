use super::*;

pub(super) fn status_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let mut status = state.application.player.status().map_err(internal_error)?;
    if let Ok(playback) = state.application.queries.playback_state_snapshot()
        && let Some(request) = playback.active_request
    {
        let active_key = request.track.as_ref().map(|track| &track.track_ref.key);
        let current_key = status
            .current_track
            .as_ref()
            .map(|track| &track.track_ref.key);
        if current_key.is_some() && current_key == active_key {
            status.requester = request.requester;
        }
    }
    serde_json::to_string(&status).map_err(internal_error)
}

pub(super) fn playback_insights_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let playback = state
        .application
        .queries
        .playback_state_snapshot()
        .map_err(internal_error)?;
    let keys: Vec<_> = playback
        .previous_requests
        .iter()
        .filter_map(|request| {
            request
                .track
                .as_ref()
                .map(|track| track.track_ref.key.clone())
        })
        .collect();
    let (cache, tracks) = state
        .application
        .player
        .cache_stats(&keys)
        .map_err(internal_error)?;
    let cached_by_key: std::collections::HashMap<_, _> = tracks
        .into_iter()
        .map(|track| ((track.source.clone(), track.id.clone()), track))
        .collect();
    let history: Vec<_> = playback
        .previous_requests
        .into_iter()
        .rev()
        .map(|request| {
            let statistics = request.track.as_ref().and_then(|track| {
                let key = &track.track_ref.key;
                cached_by_key.get(&(key.provider.to_string(), key.id.clone()))
            });
            let cached = statistics.is_some_and(|track| track.cached);
            let cache_bytes = statistics.and_then(|track| track.bytes);
            json!({
                "keyword": request.keyword,
                "source": request.source,
                "title": request.title,
                "artist": request.artist,
                "requester": request.requester,
                "startedAtMs": request.started_at_ms,
                "track": request.track,
                "cached": cached,
                "cacheBytes": cache_bytes,
                "playCount": statistics.map_or(0, |track| track.play_count),
                "requestedPlayCount": statistics.map_or(0, |track| track.requested_play_count),
                "poolPlayCount": statistics.map_or(0, |track| track.pool_play_count),
                "cacheHitCount": statistics.map_or(0, |track| track.cache_hit_count),
                "failureCount": statistics.map_or(0, |track| track.failure_count),
                "lastPlayedAtMs": statistics.and_then(|track| track.last_played_at_ms),
                "lastFailureCode": statistics.and_then(|track| track.last_failure_code.as_deref()),
            })
        })
        .collect();
    serde_json::to_string(&json!({
        "history": history,
        "cache": cache,
    }))
    .map_err(internal_error)
}

/// 磁盘缓存歌曲列表页大小上限。
const CACHE_TRACKS_MAX_LIMIT: usize = 500;
/// 磁盘缓存歌曲列表默认页大小。
const CACHE_TRACKS_DEFAULT_LIMIT: usize = 100;

pub(super) fn playback_statistics_reset_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let provider = query_value(query, "provider")
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| bad_request("缺少provider参数"))?
        .parse::<ProviderId>()
        .map_err(|_| bad_request("provider参数无效"))?;
    let id = query_value(query, "id")
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| bad_request("缺少id参数"))?;
    let key = TrackKey::new(provider, id).map_err(|_| bad_request("id参数无效"))?;
    let reset = state
        .application
        .player
        .reset_track_statistics(&key)
        .map_err(internal_error)?;
    serde_json::to_string(&json!({
        "ok": true,
        "reset": reset,
        "cachePreserved": true,
        "metadataPreserved": true,
    }))
    .map_err(internal_error)
}

/// POST /playback/song/delete?provider=&id= :删除指定歌曲
/// (从播放池移除,不再随机播放;同时删除磁盘音频缓存)。
pub(super) fn playback_song_delete_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let provider = query_value(query, "provider")
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| bad_request("缺少provider参数"))?
        .parse::<ProviderId>()
        .map_err(|_| bad_request("provider参数无效"))?;
    let id = query_value(query, "id")
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| bad_request("缺少id参数"))?;
    let key = TrackKey::new(provider, id).map_err(|_| bad_request("id参数无效"))?;
    // 1) 从播放池移除(不再参与随机播放)。
    let pool_removed = match state
        .application
        .tasks
        .apply_mutation(BusinessMutationIntent::Playback(
            PlaybackMutationIntent::RemovePoolTrack(key.clone()),
        ))
        .map_err(internal_error)?
    {
        BusinessMutationOutcome::Playback(PlaybackMutationOutcome::PoolTrackRemoved(removed)) => {
            removed
        }
        _ => {
            return Err(internal_error("播放池删除结果变体不匹配"));
        }
    };
    // 2) 删除磁盘音频缓存(下次播放重新下载)。
    let cache_removed = state
        .application
        .player
        .invalidate_track_cache(&key)
        .map_err(internal_error)?;
    serde_json::to_string(&json!({
        "ok": true,
        "poolRemoved": pool_removed,
        "cacheRemoved": cache_removed,
    }))
    .map_err(internal_error)
}

/// POST /playback/cache/invalidate?provider=&id= :删除指定曲目的磁盘音频缓存。
pub(super) fn playback_cache_invalidate_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let provider = query_value(query, "provider")
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| bad_request("缺少provider参数"))?
        .parse::<ProviderId>()
        .map_err(|_| bad_request("provider参数无效"))?;
    let id = query_value(query, "id")
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| bad_request("缺少id参数"))?;
    let key = TrackKey::new(provider, id).map_err(|_| bad_request("id参数无效"))?;
    let removed = state
        .application
        .player
        .invalidate_track_cache(&key)
        .map_err(internal_error)?;
    serde_json::to_string(&json!({
        "ok": true,
        "removed": removed,
        "cachePreserved": false,
    }))
    .map_err(internal_error)
}

/// /playback/seek?position=<秒> :跳转到指定播放位置。
pub(super) fn playback_seek_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let position = query_value(query, "position")
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| bad_request("缺少position参数(秒)"))?;
    let position: f64 = position
        .trim()
        .parse()
        .map_err(|_| bad_request("position参数必须是数字(秒)"))?;
    if !position.is_finite() || position < 0.0 {
        return Err(bad_request("position参数必须是有限的非负数"));
    }
    state
        .application
        .player
        .seek(position)
        .map_err(internal_error)?;
    serde_json::to_string(&json!({ "ok": true })).map_err(internal_error)
}

/// GET /playback/mode 查询当前播放模式;
/// POST /playback/mode?mode=sequential|repeat_one|shuffle 设置。
pub(super) fn playback_mode_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let Some(mode_text) = query_value(query, "mode").filter(|value| !value.trim().is_empty())
    else {
        let mode = state
            .application
            .player
            .play_mode()
            .map_err(internal_error)?;
        let label = match mode {
            1 => "repeat_one",
            2 => "shuffle",
            _ => "sequential",
        };
        return serde_json::to_string(&json!({ "mode": label })).map_err(internal_error);
    };
    let mode = match mode_text.trim() {
        "sequential" | "0" => 0,
        "repeat_one" | "1" => 1,
        "shuffle" | "2" => 2,
        _ => return Err(bad_request("mode只允许sequential/repeat_one/shuffle")),
    };
    state
        .application
        .player
        .set_play_mode(mode)
        .map_err(internal_error)?;
    let label = match mode {
        1 => "repeat_one",
        2 => "shuffle",
        _ => "sequential",
    };
    serde_json::to_string(&json!({ "ok": true, "mode": label })).map_err(internal_error)
}

pub(super) fn playback_cache_tracks_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let offset = parse_page_param(query_value(query, "offset"), "offset", 0)?;
    let limit = parse_page_param(
        query_value(query, "limit"),
        "limit",
        CACHE_TRACKS_DEFAULT_LIMIT,
    )?;
    if limit == 0 {
        return Err(bad_request("limit参数必须是1-500"));
    }
    let limit = limit.min(CACHE_TRACKS_MAX_LIMIT);
    let page = state
        .application
        .player
        .cached_tracks(offset, limit)
        .map_err(internal_error)?;
    serde_json::to_string(&page).map_err(internal_error)
}

/// 解析分页参数：缺省返回 `default`；非整数（含负数）返回 bad request。
fn parse_page_param(
    value: Option<&str>,
    name: &str,
    default: usize,
) -> std::result::Result<usize, AppError> {
    let Some(value) = value.filter(|value| !value.trim().is_empty()) else {
        return Ok(default);
    };
    value
        .parse::<usize>()
        .map_err(|_| bad_request(&format!("{name}参数必须是整数")))
}

pub(super) fn play_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    state
        .application
        .commands
        .playback_control(PlaybackCommand::Resume)
        .map_err(command_error)
}

pub(super) fn pause_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    state
        .application
        .commands
        .playback_control(PlaybackCommand::Pause)
        .map_err(command_error)
}

pub(super) fn skip_next_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    state
        .application
        .commands
        .playback_control(PlaybackCommand::Next)
        .map_err(command_error)
}

pub(super) fn skip_prev_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    state
        .application
        .commands
        .playback_control(PlaybackCommand::Previous)
        .map_err(command_error)
}

pub(super) fn volume_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let volume =
        query_value(query, "volume").ok_or_else(|| bad_request("volume参数必须是0-100"))?;
    if !is_valid_volume(volume) {
        return Err(bad_request("volume参数必须是0-100"));
    }
    state
        .application
        .commands
        .playback_control(PlaybackCommand::Volume(volume.to_string()))
        .map_err(command_error)
}

pub(super) fn search_play_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_remote_song(query, state, false)
}

pub(super) fn search_source_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    // 指定音源点歌：source 必填，与 /searchPlay 区分。
    if query_value(query, "source")
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
    {
        return Err(bad_request("指定音源点歌必须提供 source 参数"));
    }
    enqueue_remote_song(query, state, false)
}

pub(super) fn search_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let keyword = normalize_keyword(query_value(query, "keyword"))?;
    let source = normalize_optional_source(query_value(query, "source"))?;
    state
        .application
        .player
        .search_text(&keyword, &source)
        .map_err(player_search_error)
}

pub(super) fn search_candidates_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let keyword = normalize_keyword(query_value(query, "keyword"))?;
    let source = normalize_optional_source(query_value(query, "source"))?;
    serde_json::to_string(
        &state
            .application
            .player
            .search_candidates(&keyword, &source)
            .map_err(player_search_error)?,
    )
    .map_err(internal_error)
}

pub(super) fn player_providers_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    serde_json::to_string(
        &state
            .application
            .login
            .providers()
            .map_err(login_http_error)?,
    )
    .map_err(internal_error)
}

pub(super) fn player_login_status_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    serde_json::to_string(&state.application.login.status()).map_err(internal_error)
}

pub(super) fn player_login_refresh_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let provider = query_value(query, "provider")
        .unwrap_or("kugou")
        .parse::<ProviderId>()
        .map_err(|_| bad_request("provider参数无效"))?;
    serde_json::to_string(
        &state
            .application
            .login
            .refresh(provider)
            .map_err(login_http_error)?,
    )
    .map_err(internal_error)
}

pub(super) fn player_kugou_status_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    serde_json::to_string(
        &state
            .application
            .login
            .kugou_status()
            .map_err(login_http_error)?,
    )
    .map_err(internal_error)
}

pub(super) fn player_account_status_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let provider = query_value(query, "provider")
        .unwrap_or("netease")
        .parse::<ProviderId>()
        .map_err(|_| bad_request("provider参数无效"))?;
    serde_json::to_string(
        &state
            .application
            .login
            .account_status(provider)
            .map_err(login_http_error)?,
    )
    .map_err(internal_error)
}

pub(super) fn player_kugou_claim_vip_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    serde_json::to_string(
        &state
            .application
            .login
            .kugou_claim_vip()
            .map_err(login_http_error)?,
    )
    .map_err(internal_error)
}

pub(super) fn player_kugou_upgrade_vip_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    serde_json::to_string(
        &state
            .application
            .login
            .kugou_upgrade_vip()
            .map_err(login_http_error)?,
    )
    .map_err(internal_error)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LoginProviderRequest {
    provider: ProviderId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LoginCancelRequest {
    session_id: Uuid,
}

pub(super) fn player_play_track_body_route(
    body: &[u8],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let request = parse_json_body::<PlayTrackRequest>(body, "结构化曲目")?;
    let requester = normalize_optional_text(Some(&request.requester), "requester")?;
    state
        .application
        .commands
        .play_track(PlayTrackRequest {
            requester,
            ..request
        })
        .map_err(command_error)
}

pub(super) fn player_login_start_body_route(
    body: &[u8],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let request = parse_json_body::<LoginProviderRequest>(body, "登录请求")?;
    let session = state
        .application
        .login
        .start(request.provider)
        .map_err(login_http_error)?;
    Ok(json!({
        "ok": true,
        "sessionId": session.session_id,
        "provider": session.provider,
    })
    .to_string())
}

pub(super) fn player_login_cancel_body_route(
    body: &[u8],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let request = parse_json_body::<LoginCancelRequest>(body, "取消登录请求")?;
    state
        .application
        .login
        .cancel(request.session_id)
        .map_err(login_http_error)?;
    Ok(json!({ "ok": true, "sessionId": request.session_id }).to_string())
}

pub(super) fn player_logout_body_route(
    body: &[u8],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let request = parse_json_body::<LoginProviderRequest>(body, "退出登录请求")?;
    let status = state
        .application
        .login
        .logout(request.provider)
        .map_err(login_http_error)?;
    serde_json::to_string(&json!({ "ok": true, "credential": status })).map_err(internal_error)
}

pub(super) fn queue_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    queue_json(state)
}

pub(super) fn queue_add_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    queue_add(query, state)
}

pub(super) fn queue_remove_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    queue_remove(query, state)
}

pub(super) fn queue_clear_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    queue_clear(state)
}

pub(super) fn enqueue_remote_song(
    query: &[(String, String)],
    state: &HttpSharedState,
    ai_assisted: bool,
) -> std::result::Result<String, AppError> {
    let keyword = normalize_keyword(query_value(query, "keyword"))?;
    let source = normalize_source(query_value(query, "source"))?;
    let prefer_accompaniment = parse_bool(query_value_or(
        query,
        "preferAccompaniment",
        "accompaniment",
    ));
    state
        .application
        .commands
        .remote_song(keyword, source, prefer_accompaniment, ai_assisted)
        .map_err(command_error)
}

pub(super) fn queue_json(state: &HttpSharedState) -> std::result::Result<String, AppError> {
    serde_json::to_string(
        &state
            .application
            .queries
            .playback_queue_snapshot()
            .map_err(internal_error)?,
    )
    .map_err(internal_error)
}

pub(super) fn queue_add(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let keyword = normalize_keyword(query_value(query, "keyword"))?;
    let source = normalize_source(query_value(query, "source"))?;
    let prefer = parse_bool(query_value_or(
        query,
        "preferAccompaniment",
        "accompaniment",
    ));
    let ai_original_text =
        normalize_optional_text(query_value(query, "aiOriginalText"), "aiOriginalText")?;
    let requester = requester_from_query(query)?;
    let item = QueueItem {
        id: 0,
        keyword,
        source,
        prefer_accompaniment: prefer,
        ai_original_text,
        track: None,
        friend_username: String::new(),
        requester,
        // 与聊天路径一致：HTTP 默认不绕过去重；协议无权限/参数，不提供显式 bypass。
        dedup_bypass: false,
        candidate_snapshot: Vec::new(),
    };
    // 入队前复用播放队列统一去重策略，避免直接允许明显重复。
    if state
        .application
        .tasks
        .playback_queue_contains(item.clone())
        .map_err(internal_error)?
    {
        return Err(AppError {
            status: 409,
            message: format!("队列已有: {}", item.keyword),
        });
    }
    let BusinessMutationOutcome::Playback(PlaybackMutationOutcome::Pushed(pushed)) = state
        .application
        .tasks
        .apply_mutation(BusinessMutationIntent::Playback(
            PlaybackMutationIntent::Push(Box::new(item)),
        ))
        .map_err(internal_error)?
    else {
        return Err(internal_error(
            "playback queue push intent returned a different outcome",
        ));
    };
    if !pushed.accepted {
        return Err(AppError {
            status: 400,
            message: "队列已满".to_string(),
        });
    }
    Ok(json!({ "ok": true, "size": pushed.size }).to_string())
}

pub(super) fn requester_from_query(
    query: &[(String, String)],
) -> std::result::Result<String, AppError> {
    let requester = normalize_optional_text(query_value(query, "requester"), "requester")?;
    Ok(if requester.is_empty() {
        "WEB/API".to_string()
    } else {
        requester
    })
}

pub(super) fn queue_remove(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let removal = if let Some(id_text) = query_value(query, "id").filter(|value| !value.is_empty())
    {
        let id = id_text
            .parse::<u64>()
            .ok()
            .filter(|id| *id > 0)
            .ok_or_else(|| bad_request("无效的队列项ID"))?;
        QueueRemoval::Id(id)
    } else if let Some(index_text) = query_value(query, "index") {
        if !index_text.is_empty() {
            let index = index_text
                .parse::<usize>()
                .map_err(|_| bad_request("无效的队列索引"))?;
            QueueRemoval::Index(index)
        } else {
            QueueRemoval::Front
        }
    } else {
        QueueRemoval::Front
    };
    let BusinessMutationOutcome::Playback(PlaybackMutationOutcome::Removed(removed)) = state
        .application
        .tasks
        .apply_mutation(BusinessMutationIntent::Playback(
            PlaybackMutationIntent::Remove(removal),
        ))
        .map_err(internal_error)?
    else {
        return Err(internal_error(
            "playback queue remove intent returned a different outcome",
        ));
    };
    let QueueRemoveOutcome::Removed { index, item, size } = removed else {
        return Err(match removed {
            QueueRemoveOutcome::MissingId => AppError {
                status: 409,
                message: "队列已发生变化，请刷新后重试".to_string(),
            },
            QueueRemoveOutcome::InvalidIndex => bad_request("无效的队列索引"),
            QueueRemoveOutcome::Empty => bad_request("队列为空"),
            QueueRemoveOutcome::Removed { .. } => AppError {
                status: 500,
                message: "内部错误: 队列移除结果变体不匹配".to_string(),
            },
        });
    };
    Ok(json!({
        "ok": true,
        "size": size,
        "removed": {
            "index": index,
            "id": item.id,
            "keyword": item.keyword,
        }
    })
    .to_string())
}

pub(super) fn queue_clear(state: &HttpSharedState) -> std::result::Result<String, AppError> {
    let BusinessMutationOutcome::Playback(PlaybackMutationOutcome::Cleared) = state
        .application
        .tasks
        .apply_mutation(BusinessMutationIntent::Playback(
            PlaybackMutationIntent::Clear,
        ))
        .map_err(internal_error)?
    else {
        return Err(internal_error(
            "playback queue clear intent returned a different outcome",
        ));
    };
    Ok(json!({ "ok": true }).to_string())
}
