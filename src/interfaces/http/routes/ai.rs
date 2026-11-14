use super::*;

pub(super) fn ai_recognize_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    state
        .application
        .ai
        .recognize(query)
        .map_err(ai_route_error)
}

pub(super) fn ai_match_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    state
        .application
        .ai
        .match_song(query)
        .map_err(ai_route_error)
}

pub(super) fn ai_pick_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    state.application.ai.pick(query).map_err(ai_route_error)
}

pub(super) fn ai_search_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_remote_song(query, state, true)
}

/// GET /ai/models：拉取当前 AI 配置可用的模型列表。
///
/// 只读：不改任何配置。面板据此渲染"选择模型"下拉框，
/// 用户不必手敲模型名，也不会因为拼错而静默失败。
pub(super) fn ai_models_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let config = state
        .config_store
        .lock()
        .map_err(|_| internal_error(anyhow!("配置存储锁已损坏")))?
        .load_full()
        .map_err(internal_error)?
        .ai;
    let models = crate::config::ai::fetch_models(&config).map_err(model_list_error)?;
    serde_json::to_string(&json!({ "models": models })).map_err(internal_error)
}

/// POST /ai/models 的可选连接参数覆盖。
///
/// 面板上"获取模型列表"就在参数输入框旁边，用户不会先保存再点。
/// 只读已保存配置会让刚填的 endpoint/key 被忽略、请求打到旧网关，
/// 所以这里接受表单当前值。
///
/// 字段缺失（None）表示沿用已保存配置，空字符串表示"面板上就是空的"；
/// 两者必须区分，否则用户清空 endpoint 想用服务商默认地址时会悄悄回落到旧网关。
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AiModelsOverrides {
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    responses_endpoint: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    http_proxy: Option<String>,
    #[serde(default)]
    wire_api: Option<crate::config::ai::WireApi>,
}

/// 把面板当前值合并到已保存配置上；字段缺失表示沿用已保存值。
pub(super) fn apply_ai_model_overrides(
    config: &mut crate::config::ai::AiConfig,
    overrides: AiModelsOverrides,
) {
    if let Some(value) = overrides.provider {
        config.provider = value;
    }
    if let Some(value) = overrides.endpoint {
        config.endpoint = value;
    }
    if let Some(value) = overrides.responses_endpoint {
        config.responses_endpoint = value;
    }
    if let Some(value) = overrides.api_key {
        config.api_key = value;
    }
    if let Some(value) = overrides.http_proxy {
        config.http_proxy = value;
    }
    if let Some(value) = overrides.wire_api {
        config.wire_api = value;
    }
}

/// POST /ai/models：用面板当前值（未保存也算）拉取模型列表。
pub(super) fn ai_models_body_route(
    body: &[u8],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let overrides: AiModelsOverrides = parse_json_body(body, "模型列表参数")?;
    let mut config = state
        .config_store
        .lock()
        .map_err(|_| internal_error(anyhow!("配置存储锁已损坏")))?
        .load_full()
        .map_err(internal_error)?
        .ai;
    apply_ai_model_overrides(&mut config, overrides);
    let models = crate::config::ai::fetch_models(&config).map_err(model_list_error)?;
    serde_json::to_string(&json!({ "models": models })).map_err(internal_error)
}

/// GET /tts/models：用已保存的 tts 配置拉 fish 音色列表 + TTS 模型 fallback。
pub(super) fn tts_models_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let config = state
        .config_store
        .lock()
        .map_err(|_| internal_error(anyhow!("配置存储锁已损坏")))?
        .load_full()
        .map_err(internal_error)?
        .tts;
    let catalog = crate::features::tts::fetch_catalog(
        &config,
        &crate::features::tts::TtsCatalogOverrides::default(),
    )
    .map_err(tts_catalog_error)?;
    serde_json::to_string(&catalog).map_err(internal_error)
}

/// POST /tts/models 的未保存配置覆盖；`None` 沿用已保存值，空串表示明确清空。
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TtsModelsOverrides {
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
}

/// 把面板当前值合并成 catalog 查询覆盖；纯函数便于测试。
pub(super) fn tts_catalog_overrides(
    overrides: TtsModelsOverrides,
) -> crate::features::tts::TtsCatalogOverrides {
    let mut api_key = overrides.api_key;
    // 换 endpoint 时不能把已保存的 key 发到新地址；明确清空，走 fallback。
    if overrides.endpoint.is_some() && api_key.is_none() {
        api_key = Some(String::new());
    }
    crate::features::tts::TtsCatalogOverrides {
        endpoint: overrides.endpoint,
        api_key,
    }
}

/// POST /tts/models：面板未保存的 endpoint/api_key 也能查询。
pub(super) fn tts_models_body_route(
    body: &[u8],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let overrides: TtsModelsOverrides = parse_json_body(body, "TTS 模型列表参数")?;
    let overrides = tts_catalog_overrides(overrides);
    let config = state
        .config_store
        .lock()
        .map_err(|_| internal_error(anyhow!("配置存储锁已损坏")))?
        .load_full()
        .map_err(internal_error)?
        .tts;
    let catalog =
        crate::features::tts::fetch_catalog(&config, &overrides).map_err(tts_catalog_error)?;
    serde_json::to_string(&catalog).map_err(internal_error)
}

pub(super) fn tts_catalog_error(error: crate::features::tts::TtsCatalogError) -> AppError {
    match error {
        crate::features::tts::TtsCatalogError::Config(message) => AppError {
            status: 400,
            message,
        },
        crate::features::tts::TtsCatalogError::Auth(message) => AppError {
            status: 401,
            message,
        },
        crate::features::tts::TtsCatalogError::RateLimited(message) => AppError {
            status: 429,
            message,
        },
        crate::features::tts::TtsCatalogError::Timeout(message) => AppError {
            status: 504,
            message,
        },
        crate::features::tts::TtsCatalogError::Request(message) => internal_error(anyhow!(message)),
    }
}

/// GET /ai/vision/status：返回 AI 识图模型能力状态。
///
/// 只读、永远 200：查不到/网络失败按“未知、不支持图片”返回，供面板提示降级，
/// 不把网络错误变成 500 阻塞配置页；不返回/记录任何图片或 base64。
pub(super) fn ai_vision_status_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let config = state
        .config_store
        .lock()
        .map_err(|_| internal_error(anyhow!("配置存储锁已损坏")))?
        .load_full()
        .map_err(internal_error)?;
    let model = ai_vision_model(&config.ai_chat, &config.ai);
    let payload = if !config.ai_chat.vision.enabled {
        disabled_vision_status_payload(
            &model,
            &config.ai_chat.vision.capture_scope,
            config.ai_chat.vision.allow_full_screen,
        )
    } else {
        let capabilities = if model.is_empty() {
            Err(crate::config::ai::ModelListError::Config(
                "未配置视觉/聊天模型".to_string(),
            ))
        } else {
            crate::config::ai::fetch_model_capabilities(&config.ai, &model)
        };
        vision_status_payload(
            true,
            &model,
            &config.ai_chat.vision.capture_scope,
            config.ai_chat.vision.allow_full_screen,
            capabilities,
        )
    };
    serde_json::to_string(&payload).map_err(internal_error)
}

/// POST /ai/vision/status 的未保存参数覆盖；字段缺失=沿用已保存值。
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AiVisionStatusOverrides {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
}

/// 合并面板当前值到已保存配置上；纯函数便于测试。
pub(super) fn apply_ai_vision_status_overrides(
    ai: &mut crate::config::ai::AiConfig,
    ai_chat: &mut crate::features::ai_chat::AiChatConfig,
    overrides: AiVisionStatusOverrides,
) -> Option<String> {
    if let Some(value) = overrides.provider {
        ai.provider = value;
    }
    if let Some(value) = overrides.endpoint {
        ai.endpoint = value;
    }
    if let Some(value) = overrides.api_key {
        ai.api_key = value;
    }
    overrides.model.map(|model| {
        ai_chat.vision.model = model.clone();
        ai_chat.model = model.clone();
        model
    })
}

/// POST /ai/vision/status：用面板当前未保存值查询能力，返回与 GET 相同 shape。
pub(super) fn ai_vision_status_body_route(
    body: &[u8],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let overrides: AiVisionStatusOverrides = parse_json_body(body, "AI 识图状态参数")?;
    let mut config = state
        .config_store
        .lock()
        .map_err(|_| internal_error(anyhow!("配置存储锁已损坏")))?
        .load_full()
        .map_err(internal_error)?;
    let model_override = apply_ai_vision_status_overrides(
        &mut config.ai,
        &mut config.ai_chat,
        overrides,
    );
    let model = model_override.unwrap_or_else(|| ai_vision_model(&config.ai_chat, &config.ai));
    let payload = if !config.ai_chat.vision.enabled {
        disabled_vision_status_payload(
            &model,
            &config.ai_chat.vision.capture_scope,
            config.ai_chat.vision.allow_full_screen,
        )
    } else {
        let capabilities = if model.is_empty() {
            Err(crate::config::ai::ModelListError::Config(
                "未配置视觉/聊天模型".to_string(),
            ))
        } else {
            crate::config::ai::fetch_model_capabilities(&config.ai, &model)
        };
        vision_status_payload(
            true,
            &model,
            &config.ai_chat.vision.capture_scope,
            config.ai_chat.vision.allow_full_screen,
            capabilities,
        )
    };
    serde_json::to_string(&payload).map_err(internal_error)
}

fn ai_vision_model(
    ai_chat: &crate::features::ai_chat::AiChatConfig,
    ai: &crate::config::ai::AiConfig,
) -> String {
    let configured = ai_chat.vision.model.trim();
    if !configured.is_empty() {
        return configured.to_string();
    }
    let chat = ai_chat.model.trim();
    if !chat.is_empty() {
        return chat.to_string();
    }
    ai.model.trim().to_string()
}

fn disabled_vision_status_payload(
    model: &str,
    capture_scope: &crate::features::ai_chat::VisionCaptureScope,
    allow_full_screen: bool,
) -> serde_json::Value {
    let capture_scope = match capture_scope {
        crate::features::ai_chat::VisionCaptureScope::PrimaryScreen => "primary_screen",
    };
    serde_json::json!({
        "enabled": false,
        "disabled": true,
        "model": model,
        "supports_image": false,
        "capability": "disabled",
        "source": "unknown",
        "reason": "识图未启用，未查询模型能力",
        "capture_scope": capture_scope,
        "allow_full_screen": allow_full_screen
    })
}

pub(super) fn vision_status_payload(
    enabled: bool,
    model: &str,
    capture_scope: &crate::features::ai_chat::VisionCaptureScope,
    allow_full_screen: bool,
    capabilities: std::result::Result<
        crate::config::ai::ModelCapabilities,
        crate::config::ai::ModelListError,
    >,
) -> serde_json::Value {
    let (supports_image, capability, source, reason) = match capabilities {
        Ok(capabilities) => match capabilities.supports_image {
            Some(true) => (true, "supported", "api", "模型 API 声明支持图片"),
            Some(false) => (false, "unsupported", "api", "模型 API 声明不支持图片"),
            None => (
                false,
                "unknown",
                "unknown",
                "模型 API 未返回图片能力字段，能力未知",
            ),
        },
        Err(_) => (
            false,
            "unknown",
            "unknown",
            "查询模型能力失败，能力未知",
        ),
    };
    let capture_scope = match capture_scope {
        crate::features::ai_chat::VisionCaptureScope::PrimaryScreen => "primary_screen",
    };
    serde_json::json!({
        "enabled": enabled,
        "disabled": false,
        "model": model,
        "supports_image": supports_image,
        "capability": capability,
        "source": source,
        "reason": reason,
        "capture_scope": capture_scope,
        "allow_full_screen": allow_full_screen
    })
}

/// 配置形状问题按 400 回原文（用户能照着改），上游/网络问题按 500 只记日志。
///
/// 不用 `is_client_error` 的关键字匹配：这类报错是"地址后缀不对""缺少 apiKey"
/// 之类的新文案，关键字表追不上就会把可操作的提示降级成"内部错误"。
pub(super) fn model_list_error(error: ModelListError) -> AppError {
    match error {
        ModelListError::Config(message) => AppError {
            status: 400,
            message,
        },
        ModelListError::Request(message) => internal_error(anyhow!(message)),
    }
}

/// GET /ai/memory/actors —— 列出有记忆的玩家。
pub(super) fn ai_memory_actors_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    state.application.ai.memory_actors().map_err(ai_route_error)
}

/// GET /ai/memory/actor?name=… —— 读取某个玩家的完整记忆。
pub(super) fn ai_memory_actor_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let actor = normalize_required_text(query_value(query, "name"), "name")?;
    state
        .application
        .ai
        .memory_actor(&actor)
        .map_err(ai_route_error)
}

/// GET /ai/memory/hall —— 读取大厅共享记忆。
pub(super) fn ai_memory_hall_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    state.application.ai.memory_hall().map_err(ai_route_error)
}

/// 记忆接口的请求体。
///
/// 用 JSON body 而不是 query：记忆条目是任意自然语言，URL 编码容易在特殊
/// 字符上出错，而且 POST 的语义本来就该带 body。
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MemoryTarget {
    #[serde(default)]
    name: String,
    #[serde(default)]
    fact: String,
}

fn memory_target(body: &[u8]) -> std::result::Result<MemoryTarget, AppError> {
    if body.is_empty() {
        return Ok(MemoryTarget::default());
    }
    parse_json_body(body, "记忆参数")
}

/// POST /ai/memory/forget-actor —— 删除某个玩家的记忆。
pub(super) fn ai_memory_forget_actor_route(
    body: &[u8],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let target = memory_target(body)?;
    let actor = normalize_required_text(Some(target.name.as_str()), "name")?;
    state
        .application
        .ai
        .forget_actor(&actor)
        .map_err(ai_route_error)
}

/// POST /ai/memory/delete-fact —— 删除某条事实。
pub(super) fn ai_memory_delete_fact_route(
    body: &[u8],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let target = memory_target(body)?;
    let actor = normalize_required_text(Some(target.name.as_str()), "name")?;
    let fact = normalize_required_text(Some(target.fact.as_str()), "fact")?;
    state
        .application
        .ai
        .delete_fact(&actor, &fact)
        .map_err(ai_route_error)
}

/// POST /ai/memory/forget-hall —— 清空大厅共享记忆。
pub(super) fn ai_memory_forget_hall_route(
    _body: &[u8],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    state.application.ai.forget_hall().map_err(ai_route_error)
}

/// POST /ai/memory/rebuild —— 用新规则重放历史，重建某个玩家的事实表。
pub(super) fn ai_memory_rebuild_route(
    body: &[u8],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let target = memory_target(body)?;
    let actor = normalize_required_text(Some(target.name.as_str()), "name")?;
    state
        .application
        .ai
        .rebuild_facts(&actor)
        .map_err(ai_route_error)
}

fn ai_route_error(error: anyhow::Error) -> AppError {
    // 内部错误(500)只记日志,响应用通用文案,避免错误链泄漏内部路径;
    // 仅用户输入类错误(400)保留原文。
    if is_client_error(&error.to_string()) {
        return AppError {
            status: 400,
            message: error.to_string(),
        };
    }
    internal_error(error)
}

/// GET /ai/trace —— 最近 N 次实际发往 provider 的 AI 请求摘要。
///
/// 只读内存环形缓冲，不触发任何网络调用，也不改配置。正文按条读取，
/// 列表只给摘要，避免一次把几十份提示词全塞给面板。
pub(super) fn ai_trace_route(
    _query: &[(String, String)],
    _state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    Ok(crate::ai_trace::list_json())
}

/// GET /ai/trace/entry?id=N —— 单条请求的完整报文。
///
/// 返回 system prompt、messages（含 tool 消息）、tools、参数与原始 usage；
/// 图片数据与密钥字段在入库时已脱敏。条目滚出内存后返回 404。
pub(super) fn ai_trace_entry_route(
    query: &[(String, String)],
    _state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let raw = query_value(query, "id")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| bad_request("缺少 id 参数"))?;
    let id: u64 = raw.parse().map_err(|_| bad_request("id 必须是正整数"))?;
    crate::ai_trace::entry_json(id).ok_or_else(|| AppError {
        status: 404,
        message: format!("没有 id={id} 的 AI 请求记录（可能已滚出内存）"),
    })
}
