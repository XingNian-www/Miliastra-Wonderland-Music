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
