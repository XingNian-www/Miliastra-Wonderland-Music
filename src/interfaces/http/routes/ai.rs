use super::*;
use serde_json::Value;

/// POST /ai/chat/preview：装配预览（dry-run）。
pub(super) fn ai_chat_preview_route(
    body: &[u8],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let body: Value = parse_json_body(body, "装配预览请求")?;
    let asker = body
        .get("asker")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("预览玩家");
    let question = body
        .get("question")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("（预览问题）");
    let sections = body
        .get("sections")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let store = state
        .config_store
        .lock()
        .map_err(|_| internal_error(anyhow::anyhow!("配置存储锁已损坏")))?;
    let config = store
        .preview_candidate(&sections)
        .map_err(|error| AppError {
            status: 400,
            message: error.to_string(),
        })?;
    drop(store);
    state
        .application
        .ai_memory
        .chat_preview(&config, asker, question)
        .map_err(ai_route_error)
}

pub(super) fn ai_recognize_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    state
        .application
        .ai_music
        .recognize(query)
        .map_err(ai_route_error)
}

pub(super) fn ai_match_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    state
        .application
        .ai_music
        .match_song(query)
        .map_err(ai_route_error)
}

pub(super) fn ai_pick_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    state
        .application
        .ai_music
        .pick(query)
        .map_err(ai_route_error)
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
    let config = super::config_routes::load_for_read_locked(state)?.ai;
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
///
/// 返回 `true` 表示"换了地址且没给新 key"，已把已保存的明文 `api_key` 清空，
/// 调用方应提示用户重新填写（与 [`tts_catalog_overrides`] 同一条不变量：
/// 换 endpoint 时不能把已保存的 key 发到新地址）。
pub(super) fn apply_ai_model_overrides(
    config: &mut crate::config::ai::AiConfig,
    overrides: AiModelsOverrides,
) -> bool {
    let AiModelsOverrides {
        provider,
        endpoint,
        responses_endpoint,
        api_key,
        http_proxy,
        wire_api,
    } = overrides;
    // 先算"地址是否真的变了"，再改 config：只要 endpoint / responses_endpoint /
    // http_proxy 中任一提交了与已保存值不同的地址，就不能复用已保存的明文密钥，
    // 否则一条 `{"endpoint":"http://attacker"}` 就能把 `ai.api_key` 发到任意外部地址。
    // 与已保存值相同（面板每次都会回传当前值）不算换地址，保持原行为。
    let address_changed = endpoint
        .as_deref()
        .is_some_and(|value| value != config.endpoint)
        || responses_endpoint
            .as_deref()
            .is_some_and(|value| value != config.responses_endpoint)
        || http_proxy
            .as_deref()
            .is_some_and(|value| value != config.http_proxy);
    if let Some(value) = provider {
        config.provider = value;
    }
    if let Some(value) = endpoint {
        config.endpoint = value;
    }
    if let Some(value) = responses_endpoint {
        config.responses_endpoint = value;
    }
    let mut dropped_saved_key = false;
    // 面板回传的 secret 掩码不是真实密钥：视为“沿用已保存值”，
    // 否则会把 •••••• 当 key 发给上游，得到 500「内部错误」。
    match api_key.filter(|value| value.trim() != crate::config::SECRET_MASK) {
        Some(value) => config.api_key = value,
        // 换地址且请求没给新 key：明确清空，宁可失败也不把明文密钥发往新地址。
        None if address_changed => {
            dropped_saved_key = !config.api_key.trim().is_empty();
            config.api_key.clear();
        }
        None => {}
    }
    if let Some(value) = http_proxy {
        config.http_proxy = value;
    }
    if let Some(value) = wire_api {
        config.wire_api = value;
    }
    dropped_saved_key
}

/// 换了地址却没给新 key 时给用户的提示（响应与日志共用，避免两处措辞漂移）。
const API_KEY_REFILL_HINT: &str =
    "；接口地址已变更，未复用已保存的 API Key，请重新填写 API Key 后重试";

/// POST /ai/models：用面板当前值（未保存也算）拉取模型列表。
pub(super) fn ai_models_body_route(
    body: &[u8],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let overrides: AiModelsOverrides = parse_json_body(body, "模型列表参数")?;
    let mut config = super::config_routes::load_for_read_locked(state)?.ai;
    let dropped_saved_key = apply_ai_model_overrides(&mut config, overrides);
    if dropped_saved_key {
        log::info!("AI 模型列表：接口地址已变更，未复用已保存的 API Key，需要重新填写");
    }
    let models = crate::config::ai::fetch_models(&config).map_err(|error| {
        let mut app_error = model_list_error(error);
        if dropped_saved_key {
            app_error.message.push_str(API_KEY_REFILL_HINT);
        }
        app_error
    })?;
    serde_json::to_string(&json!({
        "models": models,
        // 换地址后没有可用 key：面板据此提示用户重新填写，而不是以为接口挂了。
        "needs_api_key": dropped_saved_key,
    }))
    .map_err(internal_error)
}

/// GET /tts/models：用已保存的 tts 配置拉 fish 音色列表 + TTS 模型 fallback。
pub(super) fn tts_models_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let config = super::config_routes::load_for_read_locked(state)?.tts;
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
    let mut api_key = overrides
        .api_key
        .filter(|value| value.trim() != crate::config::SECRET_MASK);
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
    let config = super::config_routes::load_for_read_locked(state)?.tts;
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
    let config = super::config_routes::load_for_read_locked(state)?;
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
    let AiVisionStatusOverrides {
        model,
        provider,
        endpoint,
        api_key,
    } = overrides;
    // 先算"地址是否真的变了"，再改 ai：换地址就不能复用已保存的明文 key。
    let address_changed = endpoint
        .as_deref()
        .is_some_and(|value| value != ai.endpoint);
    if let Some(value) = provider {
        ai.provider = value;
    }
    if let Some(value) = endpoint {
        ai.endpoint = value;
    }
    // 掩码和空串都表示"面板没给新 key"，只能沿用已保存值：
    // 把 •••••• 当真实密钥去探测能力，必然 401，再被误报成"不支持识图"。
    match api_key.filter(|value| {
        let trimmed = value.trim();
        !trimmed.is_empty() && trimmed != crate::config::SECRET_MASK
    }) {
        Some(value) => ai.api_key = value,
        // 换地址且没给新 key：明确清空，宁可探测失败也不把明文密钥发往新地址。
        None if address_changed => ai.api_key.clear(),
        None => {}
    }
    model.map(|model| {
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
    let mut config = super::config_routes::load_for_read_locked(state)?;
    let saved_api_key_present = !config.ai.api_key.trim().is_empty();
    let model_override =
        apply_ai_vision_status_overrides(&mut config.ai, &mut config.ai_chat, overrides);
    // 地址变了 → 已保存的明文 key 被丢弃：要在响应里说清楚，别让面板把它当成"模型不支持识图"。
    let needs_api_key = saved_api_key_present && config.ai.api_key.trim().is_empty();
    if needs_api_key {
        log::info!("AI 识图状态：接口地址已变更，未复用已保存的 API Key，需要重新填写");
    }
    let model = model_override.unwrap_or_else(|| ai_vision_model(&config.ai_chat, &config.ai));
    let mut payload = if !config.ai_chat.vision.enabled {
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
    if needs_api_key {
        payload["needs_api_key"] = Value::Bool(true);
    }
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
        Err(_) => (false, "unknown", "unknown", "查询模型能力失败，能力未知"),
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
        ModelListError::Request(message) => AppError {
            status: 400,
            message: format!("获取模型列表失败：{message}"),
        },
    }
}

/// GET /ai/memory/actors —— 列出有记忆的玩家。
pub(super) fn ai_memory_actors_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    state
        .application
        .ai_memory
        .memory_actors()
        .map_err(ai_route_error)
}

/// GET /ai/memory/actor?name=… —— 读取某个玩家的完整记忆。
pub(super) fn ai_memory_actor_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let actor = normalize_required_text(query_value(query, "name"), "name")?;
    state
        .application
        .ai_memory
        .memory_actor(&actor)
        .map_err(ai_route_error)
}

/// GET /ai/memory/hall —— 读取大厅共享记忆。
pub(super) fn ai_memory_hall_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    state
        .application
        .ai_memory
        .memory_hall()
        .map_err(ai_route_error)
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
        .ai_memory
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
        .ai_memory
        .delete_fact(&actor, &fact)
        .map_err(ai_route_error)
}

/// POST /ai/memory/forget-hall —— 清空大厅共享记忆。
pub(super) fn ai_memory_forget_hall_route(
    _body: &[u8],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    state
        .application
        .ai_memory
        .forget_hall()
        .map_err(ai_route_error)
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
        .ai_memory
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_overrides_treat_secret_mask_as_unchanged() {
        let mut config = crate::config::ai::AiConfig::default();
        config.api_key = "saved-real-key".into();
        apply_ai_model_overrides(
            &mut config,
            AiModelsOverrides {
                provider: Some("deepseek".into()),
                endpoint: Some(String::new()),
                responses_endpoint: Some(String::new()),
                api_key: Some(crate::config::SECRET_MASK.into()),
                http_proxy: Some(String::new()),
                wire_api: None,
            },
        );
        assert_eq!(config.api_key, "saved-real-key", "掩码不能覆盖已保存密钥");
        assert_eq!(config.provider, "deepseek");
    }

    #[test]
    fn model_overrides_apply_real_key() {
        let mut config = crate::config::ai::AiConfig::default();
        config.api_key = "old".into();
        apply_ai_model_overrides(
            &mut config,
            AiModelsOverrides {
                provider: None,
                endpoint: None,
                responses_endpoint: None,
                api_key: Some("new-key".into()),
                http_proxy: None,
                wire_api: None,
            },
        );
        assert_eq!(config.api_key, "new-key");
    }

    #[test]
    fn model_overrides_clear_saved_key_when_address_changes() {
        // 请求换了 endpoint 却没给新 key：不能把已保存的明文 key 发到新地址。
        let mut config = crate::config::ai::AiConfig::default();
        config.endpoint = "https://saved.example/v1/chat/completions".into();
        config.api_key = "saved-real-key".into();
        let dropped = apply_ai_model_overrides(
            &mut config,
            AiModelsOverrides {
                endpoint: Some("http://attacker.example/v1/chat/completions".into()),
                ..Default::default()
            },
        );
        assert!(dropped, "换地址必须报告 key 已被丢弃");
        assert_eq!(config.api_key, "", "换地址不能复用已保存的 key");
        assert_eq!(
            config.endpoint,
            "http://attacker.example/v1/chat/completions"
        );

        // responses_endpoint / http_proxy 也算地址：任何一个变了都不能复用 key。
        for overrides in [
            AiModelsOverrides {
                responses_endpoint: Some("http://attacker.example/v1/responses".into()),
                ..Default::default()
            },
            AiModelsOverrides {
                http_proxy: Some("http://attacker.example:8080".into()),
                ..Default::default()
            },
        ] {
            let mut config = crate::config::ai::AiConfig::default();
            config.responses_endpoint = "https://saved.example/v1/responses".into();
            config.http_proxy = "http://127.0.0.1:7890".into();
            config.api_key = "saved-real-key".into();
            let dropped = apply_ai_model_overrides(&mut config, overrides);
            assert!(dropped);
            assert_eq!(config.api_key, "");
        }

        // 地址没变（面板每次都会回传当前值）时保持原行为：继续沿用已保存 key。
        let mut config = crate::config::ai::AiConfig::default();
        config.endpoint = "https://saved.example/v1/chat/completions".into();
        config.api_key = "saved-real-key".into();
        let dropped = apply_ai_model_overrides(
            &mut config,
            AiModelsOverrides {
                endpoint: Some("https://saved.example/v1/chat/completions".into()),
                api_key: Some(crate::config::SECRET_MASK.into()),
                ..Default::default()
            },
        );
        assert!(!dropped);
        assert_eq!(config.api_key, "saved-real-key");

        // 换地址同时给了新 key：用新 key，不回填旧值。
        let mut config = crate::config::ai::AiConfig::default();
        config.endpoint = "https://saved.example/v1".into();
        config.api_key = "saved-real-key".into();
        let dropped = apply_ai_model_overrides(
            &mut config,
            AiModelsOverrides {
                endpoint: Some("https://other.example/v1".into()),
                api_key: Some("new-key".into()),
                ..Default::default()
            },
        );
        assert!(!dropped);
        assert_eq!(config.api_key, "new-key");
    }

    #[test]
    fn vision_overrides_keep_saved_key_for_mask_or_empty_and_drop_it_on_address_change() {
        let mut ai = crate::config::ai::AiConfig::default();
        let mut ai_chat = crate::features::ai_chat::AiChatConfig::default();
        ai.endpoint = "https://saved.example/v1".into();
        ai.api_key = "saved-real-key".into();

        // 掩码 = 面板未修改 → 沿用已保存值，不能拿 •••••• 当 key 去探测能力。
        let mask: AiVisionStatusOverrides = serde_json::from_value(serde_json::json!({
            "endpoint": "https://saved.example/v1",
            "api_key": crate::config::SECRET_MASK,
        }))
        .expect("mask override");
        apply_ai_vision_status_overrides(&mut ai, &mut ai_chat, mask);
        assert_eq!(ai.api_key, "saved-real-key", "掩码应视为沿用已存值");

        // 空串 = 面板没填 → 同样沿用。
        let empty: AiVisionStatusOverrides =
            serde_json::from_str(r#"{"endpoint":"https://saved.example/v1","api_key":""}"#)
                .expect("empty override");
        apply_ai_vision_status_overrides(&mut ai, &mut ai_chat, empty);
        assert_eq!(ai.api_key, "saved-real-key", "空串应视为沿用已存值");

        // 换地址且没给新 key → 清空，不能把明文 key 发往新地址。
        let moved: AiVisionStatusOverrides =
            serde_json::from_str(r#"{"endpoint":"http://attacker.example/v1"}"#)
                .expect("endpoint override");
        apply_ai_vision_status_overrides(&mut ai, &mut ai_chat, moved);
        assert_eq!(ai.api_key, "");
        assert_eq!(ai.endpoint, "http://attacker.example/v1");

        // 换地址同时给了新 key → 用新 key。
        let mut ai = crate::config::ai::AiConfig::default();
        let mut ai_chat = crate::features::ai_chat::AiChatConfig::default();
        ai.endpoint = "https://saved.example/v1".into();
        ai.api_key = "saved-real-key".into();
        let fresh: AiVisionStatusOverrides =
            serde_json::from_str(r#"{"endpoint":"https://other.example/v1","api_key":"new-key"}"#)
                .expect("new key override");
        apply_ai_vision_status_overrides(&mut ai, &mut ai_chat, fresh);
        assert_eq!(ai.api_key, "new-key");
    }

    #[test]
    fn tts_overrides_drop_secret_mask() {
        let overrides = tts_catalog_overrides(TtsModelsOverrides {
            endpoint: None,
            api_key: Some(crate::config::SECRET_MASK.into()),
        });
        assert!(overrides.api_key.is_none(), "掩码不应作为 TTS 查询密钥");
    }
}
