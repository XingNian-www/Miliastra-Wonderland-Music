use super::*;

pub(super) fn operator_lyrics_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    state
        .application
        .commands
        .playback_control(PlaybackCommand::Lyrics)
        .map_err(command_error)
}

pub(super) fn operator_hall_detect_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    state
        .application
        .commands
        .hall_control(HallCommand::Detect)
        .map_err(command_error)
}

pub(super) fn operator_hall_time_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    state
        .application
        .commands
        .hall_control(HallCommand::Time)
        .map_err(command_error)
}

pub(super) fn operator_microphone_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    state
        .application
        .commands
        .hall_control(HallCommand::ToggleMicrophone {
            username: "控制台".to_string(),
        })
        .map_err(command_error)
}

pub(super) fn operator_commands_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let enabled = match normalize_required_text(query_value(query, "enabled"), "enabled")?
        .to_ascii_lowercase()
        .as_str()
    {
        "1" | "true" | "on" | "enable" | "enabled" => true,
        "0" | "false" | "off" | "disable" | "disabled" => false,
        _ => return Err(bad_request("enabled参数必须是1或0")),
    };
    state
        .application
        .commands
        .set_operator_commands(enabled)
        .map_err(command_error)
}

pub(super) fn operator_idle_exit_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    if let Some(enabled) = query_value(query, "enabled") {
        match enabled.trim().to_ascii_lowercase().as_str() {
            "0" | "false" | "off" | "disabled" => {
                return state
                    .application
                    .commands
                    .clear_idle_exit()
                    .map_err(command_error);
            }
            "1" | "true" | "on" | "enabled" => {}
            _ => return Err(bad_request("enabled参数必须是1或0")),
        }
    }
    let minutes = normalize_required_text(query_value(query, "minutes"), "minutes")?
        .parse::<u32>()
        .ok()
        .filter(|minutes| (15..=1440).contains(minutes))
        .ok_or_else(|| bad_request("minutes参数必须在15到1440之间"))?;
    state
        .application
        .commands
        .set_idle_exit(minutes)
        .map_err(command_error)
}

pub(super) fn operator_workflows_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    state
        .application
        .commands
        .list_workflows()
        .map_err(command_error)
}

pub(super) fn operator_workflow_run_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let name = normalize_required_text(query_value(query, "name"), "name")?;
    let args = normalize_optional_text(query_value(query, "args"), "args")?;
    state
        .application
        .commands
        .run_workflow(name, args)
        .map_err(command_error)
}
