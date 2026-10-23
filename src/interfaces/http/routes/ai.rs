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
    let models = crate::config::ai::fetch_models(&config).map_err(ai_route_error)?;
    serde_json::to_string(&json!({ "models": models })).map_err(internal_error)
}

pub(super) fn ai_route_error(error: anyhow::Error) -> AppError {
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
