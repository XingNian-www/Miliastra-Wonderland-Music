use super::*;

/// GET /commands/catalog
///
/// 返回命令注册表的完整视图，供命令面板渲染。只读，不触发重载、不写库。
pub(super) fn commands_catalog_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let enabled = state.monitor.snapshot().operational.commands_enabled;
    let view = crate::features::command_catalog::catalog_view(enabled);
    serde_json::to_string(&view).map_err(internal_error)
}
