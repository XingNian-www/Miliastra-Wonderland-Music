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

/// POST /ai/memory/forget-actor —— 删除某个玩家的记忆。
pub(super) fn ai_memory_forget_actor_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let actor = normalize_required_text(query_value(query, "name"), "name")?;
    state
        .application
        .ai
        .forget_actor(&actor)
        .map_err(ai_route_error)
}

/// POST /ai/memory/delete-fact —— 删除某条事实。
pub(super) fn ai_memory_delete_fact_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let actor = normalize_required_text(query_value(query, "name"), "name")?;
    let fact = normalize_required_text(query_value(query, "fact"), "fact")?;
    state
        .application
        .ai
        .delete_fact(&actor, &fact)
        .map_err(ai_route_error)
}

/// POST /ai/memory/forget-hall —— 清空大厅共享记忆。
pub(super) fn ai_memory_forget_hall_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    state.application.ai.forget_hall().map_err(ai_route_error)
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
