use super::*;

pub(super) fn startup_game_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_startup_game(state)
}

pub(super) fn startup_wonderland_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_startup_wonderland(state)
}

pub(super) fn enter_wonderland_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_enter_wonderland(state)
}

pub(super) fn state_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    state_json(state)
}

pub(super) fn state_save_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    state_save(query, state)
}

pub(super) fn history_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    history_json(state)
}

pub(super) fn clear_history_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    clear_history(state)
}

pub(super) fn monitor_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    monitor_json(state)
}

pub(super) fn tool_task_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let id = parse_tool_id(query)?;
    let snapshot = state
        .application
        .queries
        .diagnostic_task_snapshot(id)
        .map_err(internal_error)?
        .ok_or_else(|| AppError {
            status: 404,
            message: "Web 工具任务不存在或已过期".to_string(),
        })?;
    serde_json::to_string(&snapshot).map_err(internal_error)
}

pub(super) fn tool_templates_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let config = state.live_configs.snapshot();
    let marker_threshold = config.templates.marker_threshold;
    let mut templates = vec![
        json!({ "name": "blue-marker", "label": "蓝色聊天标志", "region": config.screen.chat_rect, "threshold": marker_threshold }),
        json!({ "name": "yellow-marker", "label": "黄色聊天标志", "region": config.screen.chat_rect, "threshold": marker_threshold }),
        json!({ "name": "pink-marker", "label": "粉色聊天标志", "region": config.screen.chat_rect, "threshold": marker_threshold }),
        json!({ "name": "friend", "label": "好友按钮", "region": config.screen.friend_rect, "threshold": marker_threshold }),
        json!({ "name": "secondary-back", "label": "二级聊天返回按钮", "region": config.screen.secondary_back_rect, "threshold": marker_threshold }),
        json!({ "name": "secondary-hall", "label": "二级当前大厅", "region": config.screen.secondary_hall_rect, "threshold": marker_threshold }),
        json!({ "name": "invite-view-star", "label": "邀请查看千星", "region": config.invite.view_star_region, "threshold": marker_threshold }),
        json!({ "name": "invite-goto-hall", "label": "邀请前往大厅", "region": config.invite.goto_hall_region, "threshold": marker_threshold }),
        json!({ "name": "invite-enter-hall", "label": "邀请进入大厅", "region": config.invite.enter_hall_region, "threshold": marker_threshold }),
        json!({ "name": "friend-panel", "label": "好友面板", "region": config.moderation.friend_panel_region, "threshold": marker_threshold }),
        json!({ "name": "friend-search-panel", "label": "好友搜索面板", "region": config.moderation.search_panel_region, "threshold": marker_threshold }),
        json!({ "name": "friend-more-settings", "label": "好友更多设置", "region": config.moderation.more_settings_region, "threshold": marker_threshold }),
        json!({ "name": "friend-block-chat", "label": "屏蔽聊天", "region": config.moderation.block_chat_region, "threshold": marker_threshold }),
        json!({ "name": "friend-blacklist", "label": "拉黑", "region": config.moderation.blacklist_region, "threshold": marker_threshold }),
        json!({ "name": "friend-confirm", "label": "好友操作确认", "region": config.moderation.confirm_region, "threshold": marker_threshold }),
        json!({ "name": "wonderland-confirm", "label": "千星确认按钮", "region": config.startup.wonderland_confirm_region, "threshold": config.startup.wonderland_confirm_threshold }),
        json!({ "name": "paimon-menu", "label": "派蒙主界面", "region": config.startup.main_ui_region, "threshold": config.startup.template_threshold }),
        json!({ "name": "wonderland-map-star", "label": "千星地图入口", "region": config.startup.wonderland_map_star_region, "threshold": config.startup.template_threshold }),
    ];
    let mut custom = config
        .custom_workflows
        .templates
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    custom.sort();
    templates.extend(custom.into_iter().map(|name| {
        json!({
            "name": name,
            "label": format!("自定义: {name}"),
            "region": null,
            "threshold": config.custom_workflows.default_threshold,
        })
    }));
    serde_json::to_string(&templates).map_err(internal_error)
}

pub(super) fn tool_ocr_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let rect = query_value(query, "rect")
        .filter(|value| !value.trim().is_empty())
        .map(parse_rect)
        .transpose()
        .map_err(|error| bad_request(&format!("rect参数无效: {error}")))?;
    // 安全加固:把 rect 裁剪到期望屏幕尺寸内,防止溢出/越界输入。
    let rect = rect.map(|rect| {
        crate::ui::geometry::clamp_rect(
            rect,
            state.config.screen.expected_width,
            state.config.screen.expected_height,
        )
    });
    enqueue_web_tool(state, WebToolRequest::Ocr { rect })
}

pub(super) fn tool_scan_chat_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_web_tool(state, WebToolRequest::ScanChat)
}

pub(super) fn tool_ui_state_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_web_tool(state, WebToolRequest::UiState)
}

pub(super) fn tool_hall_name_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_web_tool(state, WebToolRequest::HallName)
}

pub(super) fn tool_template_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let config = state.live_configs.snapshot();
    let name = normalize_required_text(query_value(query, "template"), "template")?;
    let template = WebToolTemplate::parse(&name, &config.custom_workflows.templates)
        .map_err(|error| bad_request(&error.to_string()))?;
    let rect = query_value(query, "rect")
        .filter(|value| !value.trim().is_empty())
        .map(parse_rect)
        .transpose()
        .map_err(|error| bad_request(&format!("rect参数无效: {error}")))?;
    // 安全加固:把 rect 裁剪到期望屏幕尺寸内。
    let rect = rect.map(|rect| {
        crate::ui::geometry::clamp_rect(
            rect,
            config.screen.expected_width,
            config.screen.expected_height,
        )
    });
    let threshold = query_value(query, "threshold")
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            value
                .trim()
                .parse::<f32>()
                .map_err(|_| bad_request("threshold参数必须是0到1之间的小数"))
        })
        .transpose()?;
    if threshold.is_some_and(|value| !(0.0..=1.0).contains(&value)) {
        return Err(bad_request("threshold参数必须是0到1之间的小数"));
    }
    enqueue_web_tool(
        state,
        WebToolRequest::MatchTemplate {
            template,
            rect,
            threshold,
            click: parse_bool(query_value(query, "click")),
        },
    )
}

pub(super) fn tool_click_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let x = parse_coordinate(query_value(query, "x"), "x")?;
    let y = parse_coordinate(query_value(query, "y"), "y")?;
    // 安全加固:点击坐标限制在期望屏幕范围内。
    let x = x.clamp(
        0,
        state.config.screen.expected_width.saturating_sub(1) as i32,
    );
    let y = y.clamp(
        0,
        state.config.screen.expected_height.saturating_sub(1) as i32,
    );
    enqueue_web_tool(state, WebToolRequest::Click { x, y })
}

pub(super) fn tool_key_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let key = normalize_required_text(query_value(query, "key"), "key")?;
    if key.chars().count() > 40 {
        return Err(bad_request("key参数过长"));
    }
    enqueue_web_tool(state, WebToolRequest::Key { key })
}

pub(super) fn tool_chat_change_samples_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let samples = parse_bounded_u32(query_value(query, "samples"), "samples", 1, 30, 10)?;
    let live_config = state.live_configs.snapshot();
    let interval_ms = parse_bounded_u64(
        query_value(query, "intervalMs"),
        "intervalMs",
        50,
        5_000,
        live_config.timing.loop_idle_ms,
    )?;
    enqueue_web_tool(
        state,
        WebToolRequest::ChatChangeSamples {
            samples,
            interval_ms,
        },
    )
}

pub(super) fn tool_panel_benchmark_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let rounds = parse_bounded_u32(query_value(query, "rounds"), "rounds", 1, 10, 3)?;
    enqueue_web_tool(state, WebToolRequest::PanelResponseBenchmark { rounds })
}

pub(super) fn tool_ocr_backends_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_web_tool(state, WebToolRequest::OcrBackendProbe)
}

pub(super) fn tool_ai_preview_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let keyword = normalize_keyword(query_value(query, "keyword"))?;
    let prefer_accompaniment = parse_bool(query_value_or(
        query,
        "preferAccompaniment",
        "accompaniment",
    ));
    enqueue_web_tool(
        state,
        WebToolRequest::AiSearchPreview {
            keyword,
            prefer_accompaniment,
        },
    )
}

pub(super) fn enqueue_web_tool(
    state: &HttpSharedState,
    request: WebToolRequest,
) -> std::result::Result<String, AppError> {
    let snapshot = state
        .application
        .tasks
        .enqueue_diagnostic(request)
        .map_err(|error| AppError {
            status: if error.to_string().contains("任务过多") {
                429
            } else {
                500
            },
            message: error.to_string(),
        })?;
    serde_json::to_string(&snapshot).map_err(internal_error)
}

pub(super) fn parse_tool_id(query: &[(String, String)]) -> std::result::Result<u64, AppError> {
    query_value(query, "id")
        .ok_or_else(|| bad_request("缺少id参数"))?
        .parse::<u64>()
        .map_err(|_| bad_request("id参数无效"))
}

pub(super) fn parse_coordinate(
    value: Option<&str>,
    name: &str,
) -> std::result::Result<i32, AppError> {
    normalize_required_text(value, name)?
        .parse::<i32>()
        .map_err(|_| bad_request(&format!("{}参数必须是整数", name)))
}

pub(super) fn parse_bounded_u32(
    value: Option<&str>,
    name: &str,
    min: u32,
    max: u32,
    default: u32,
) -> std::result::Result<u32, AppError> {
    let Some(value) = value.filter(|value| !value.trim().is_empty()) else {
        return Ok(default);
    };
    let parsed = value
        .parse::<u32>()
        .map_err(|_| bad_request(&format!("{}参数必须是整数", name)))?;
    if (min..=max).contains(&parsed) {
        Ok(parsed)
    } else {
        Err(bad_request(&format!(
            "{}参数必须在{}到{}之间",
            name, min, max
        )))
    }
}

pub(super) fn parse_bounded_u64(
    value: Option<&str>,
    name: &str,
    min: u64,
    max: u64,
    default: u64,
) -> std::result::Result<u64, AppError> {
    let Some(value) = value.filter(|value| !value.trim().is_empty()) else {
        return Ok(default);
    };
    let parsed = value
        .parse::<u64>()
        .map_err(|_| bad_request(&format!("{}参数必须是整数", name)))?;
    if (min..=max).contains(&parsed) {
        Ok(parsed)
    } else {
        Err(bad_request(&format!(
            "{}参数必须在{}到{}之间",
            name, min, max
        )))
    }
}

pub(super) fn health_route(
    _query: &[(String, String)],
    _state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    Ok("OK".to_string())
}

pub(super) fn enqueue_startup_game(
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_startup_task_response(
        state,
        "启动游戏",
        [StartupTask::start_game(StartupSource::REMOTE_CONSOLE)],
    )
}

pub(super) fn enqueue_enter_wonderland(
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_startup_task_response(
        state,
        "进入千星",
        [StartupTask::enter_wonderland(StartupSource::REMOTE_CONSOLE)],
    )
}

pub(super) fn enqueue_startup_wonderland(
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_startup_task_response(
        state,
        "启动游戏并进入千星",
        [
            StartupTask::start_game(StartupSource::REMOTE_CONSOLE),
            StartupTask::enter_wonderland(StartupSource::REMOTE_CONSOLE),
        ],
    )
}

pub(super) fn enqueue_startup_task_response<const N: usize>(
    state: &HttpSharedState,
    task_label: &'static str,
    tasks: [StartupTask; N],
) -> std::result::Result<String, AppError> {
    let mut receipts = Vec::with_capacity(N);
    let mut failures = Vec::new();
    for (index, task) in tasks.into_iter().enumerate() {
        match required_enqueue_receipt(state.application.tasks.enqueue_startup(task)) {
            Ok(receipt) => receipts.push(receipt),
            Err(error) => failures.push(json!({
                "index": index,
                "error": error.message,
            })),
        }
    }
    if failures.is_empty() {
        return enqueue_startup_success(task_label, &receipts);
    }
    // 部分任务入队失败：返回 200 并带完整信息，前端可感知已入队部分。
    let mut response = json!({
        "ok": true,
        "queued": !receipts.is_empty(),
        "allQueued": false,
        "task": task_label,
        "failed": failures,
    });
    if !receipts.is_empty() {
        let positions = receipts.iter().map(|r| r.position).collect::<Vec<_>>();
        let task_ids = receipts.iter().map(|r| r.task_id).collect::<Vec<_>>();
        if let Some(object) = response.as_object_mut() {
            object.insert("positions".to_string(), json!(positions));
            object.insert("taskIds".to_string(), json!(task_ids));
        }
    }
    Ok(response.to_string())
}

fn enqueue_startup_success(
    task_label: &'static str,
    receipts: &[EnqueueReceipt],
) -> std::result::Result<String, AppError> {
    let positions = receipts.iter().map(|r| r.position).collect::<Vec<_>>();
    let task_ids = receipts.iter().map(|r| r.task_id).collect::<Vec<_>>();
    let mut response = json!({
        "ok": true,
        "queued": true,
        "task": task_label,
    });
    if let Some(object) = response.as_object_mut() {
        if receipts.len() == 1 {
            object.insert("position".to_string(), json!(positions[0]));
            object.insert("taskId".to_string(), json!(task_ids[0]));
        } else {
            object.insert("positions".to_string(), json!(positions));
            object.insert("taskIds".to_string(), json!(task_ids));
        }
    }
    Ok(response.to_string())
}

pub(super) fn state_json(state: &HttpSharedState) -> std::result::Result<String, AppError> {
    let mut playback = serde_json::to_value(
        state
            .application
            .queries
            .playback_state_snapshot()
            .map_err(internal_error)?,
    )
    .map_err(internal_error)?;
    if let Some(object) = playback.as_object_mut() {
        object.remove("previousRequests");
    }
    let hall = state
        .application
        .queries
        .hall_state_snapshot()
        .map_err(internal_error)?;
    serde_json::to_string(&json!({
        "playback": playback,
        "hallRemainingMinutes": hall.remaining_minutes,
        "hallRemainingUpdatedAt": hall.remaining_updated_at,
        "hallExpiringWarningSent": hall.expiring_warning_sent,
        "hallRemainingMinutesNow": hall.remaining_minutes_now(),
    }))
    .map_err(internal_error)
}

pub(super) fn state_save(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let text = query_value(query, "json").unwrap_or("{}");
    let patch: HashMap<String, serde_json::Value> =
        serde_json::from_str(text).map_err(|error| AppError {
            status: 400,
            message: format!("json参数无效: {}", error),
        })?;
    let BusinessMutationOutcome::Hall(HallMutationOutcome::StatePatched) = state
        .application
        .tasks
        .apply_mutation(BusinessMutationIntent::Hall(
            HallMutationIntent::PatchState(hall_state_patch(&patch)?),
        ))
        .map_err(internal_error)?
    else {
        return Err(internal_error(
            "runtime state patch intent returned a different outcome",
        ));
    };
    Ok(json!({ "ok": true }).to_string())
}

pub(super) fn history_json(state: &HttpSharedState) -> std::result::Result<String, AppError> {
    let history = state
        .history
        .lock()
        .map_err(|_| internal_message("历史锁已损坏"))?;
    serde_json::to_string(&*history).map_err(internal_error)
}

pub(super) fn clear_history(state: &HttpSharedState) -> std::result::Result<String, AppError> {
    state
        .history
        .lock()
        .map_err(|_| internal_message("历史锁已损坏"))?
        .clear();
    Ok("命令记录已清空".to_string())
}

pub(super) fn screenshot_response(
    request: &Request,
    state: &HttpSharedState,
) -> std::result::Result<Response, AppError> {
    let quality = parse_jpeg_quality(query_value(&request.query, "quality"))?;
    cached_screenshot_response(
        request,
        quality,
        &state.latest_frame,
        "尚未获取主扫描画面，请稍后重试",
        &state.config.http.host,
        state.config.http.port,
    )
}

pub(super) fn hall_screenshot_response(
    request: &Request,
    state: &HttpSharedState,
) -> std::result::Result<Response, AppError> {
    let quality = parse_jpeg_quality(query_value(&request.query, "quality"))?;
    let image = state
        .application
        .hall
        .capture_hall_screenshot()
        .map_err(|error| {
            // 内部错误详情只写日志，响应体不携带内部路径/错误链。
            log::error!("主动检测大厅失败: {error:#}");
            AppError {
                status: 503,
                message: "主动检测大厅失败".to_string(),
            }
        })?;
    encoded_screenshot_response(
        request,
        quality,
        &image,
        &state.config.http.host,
        state.config.http.port,
    )
}

pub(super) fn cached_screenshot_response(
    request: &Request,
    quality: u8,
    cache: &Arc<Mutex<LatestFrameCache>>,
    unavailable_message: &str,
    host: &str,
    port: u16,
) -> std::result::Result<Response, AppError> {
    let image = cache
        .lock()
        .map_err(|_| internal_message("截图缓存锁已损坏"))?
        .image()
        .ok_or_else(|| AppError {
            status: 503,
            message: unavailable_message.to_string(),
        })?;
    encoded_screenshot_response(request, quality, &image, host, port)
}

pub(super) fn encoded_screenshot_response(
    request: &Request,
    quality: u8,
    image: &DynamicImage,
    host: &str,
    port: u16,
) -> std::result::Result<Response, AppError> {
    let rgb = image.to_rgb8();
    let mut bytes = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut bytes, quality);
    encoder
        .encode(&rgb, rgb.width(), rgb.height(), ColorType::Rgb8.into())
        .map_err(internal_error)?;
    Ok(bytes_response(
        StatusCode::OK,
        "image/jpeg",
        bytes,
        cors_headers(request, host, port),
    ))
}

pub(super) fn monitor_json(state: &HttpSharedState) -> std::result::Result<String, AppError> {
    serde_json::to_string(&state.monitor.snapshot()).map_err(internal_error)
}

pub(super) fn hall_state_patch(
    patch: &HashMap<String, serde_json::Value>,
) -> std::result::Result<HallStatePatch, AppError> {
    let remaining_minutes = match patch.get("hallRemainingMinutes") {
        None => None,
        Some(value) if value.is_null() => Some(None),
        Some(value) => Some(Some(
            value
                .as_u64()
                .and_then(|minutes| u32::try_from(minutes).ok())
                .ok_or_else(|| bad_request("hallRemainingMinutes 必须是 0-4294967295 的整数"))?,
        )),
    };
    let remaining_updated_at = match patch.get("hallRemainingUpdatedAt") {
        None => None,
        Some(value) if value.is_null() => Some(None),
        Some(value) => {
            Some(Some(value.as_u64().ok_or_else(|| {
                bad_request("hallRemainingUpdatedAt 必须是时间戳整数")
            })?))
        }
    };
    let expiring_warning_sent = match patch.get("hallExpiringWarningSent") {
        None => None,
        Some(value) => Some(
            value
                .as_bool()
                .ok_or_else(|| bad_request("hallExpiringWarningSent 必须是 true/false"))?,
        ),
    };
    Ok(HallStatePatch {
        remaining_minutes,
        remaining_updated_at,
        expiring_warning_sent,
    })
}
