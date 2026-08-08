use super::*;

pub(super) fn chat_send_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    chat_send(query, state)
}

pub(super) fn chat_listener_mode_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let mode = match normalize_required_text(query_value(query, "mode"), "mode")?.as_str() {
        "primary" | "一级" => ChatListenerMode::Primary,
        "secondary" | "二级" => ChatListenerMode::Secondary,
        _ => {
            return Err(AppError {
                status: 400,
                message: "mode 仅支持 primary 或 secondary".to_string(),
            });
        }
    };
    state
        .application
        .commands
        .request_chat_listener_mode(mode)
        .map_err(command_error)
}

pub(super) fn task_cancel_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let task_id = normalize_required_text(query_value(query, "id"), "id")?
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| bad_request("无效的任务ID"))?;
    let outcome = state
        .application
        .tasks
        .cancel_task(task_id)
        .map_err(internal_error)?;
    match outcome {
        FormalTaskCancelOutcome::CanceledBeforeStart => Ok(json!({
            "ok": true,
            "taskId": task_id,
            "canceled": true,
            "cancellationRequested": false,
        })
        .to_string()),
        FormalTaskCancelOutcome::CancellationRequested => Ok(json!({
            "ok": true,
            "taskId": task_id,
            "canceled": false,
            "cancellationRequested": true,
        })
        .to_string()),
        FormalTaskCancelOutcome::AlreadyFinished => Err(AppError {
            status: 409,
            message: "任务已经结束，最终结果不会再改变".to_string(),
        }),
        FormalTaskCancelOutcome::NotFound => Err(AppError {
            status: 404,
            message: "没有找到该任务".to_string(),
        }),
    }
}

pub(super) fn decision_submit_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let id = normalize_required_text(query_value(query, "id"), "id")?
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| bad_request("无效的决策ID"))?;
    let action_text = normalize_required_text(query_value(query, "action"), "action")?;
    let action = DecisionAction::parse(&action_text)
        .ok_or_else(|| bad_request("action仅支持confirm、skip、switch_source或ai"))?;
    state
        .application
        .tasks
        .submit_decision(id, action)
        .map_err(|error| AppError {
            status: 409,
            message: error.to_string(),
        })?;
    Ok(json!({ "ok": true, "decisionId": id, "submitted": action_text }).to_string())
}

pub(super) fn chat_send(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let text = normalize_required_text(query_value(query, "text"), "text")?;
    let use_prefix = parse_bool_default(
        query_value(query, "usePrefix")
            .or_else(|| query_value(query, "prefixEnabled"))
            .or_else(|| query_value(query, "withPrefix")),
        true,
    );
    let prefix = if use_prefix {
        normalize_optional_raw_text(
            query_value(query, "prefix").or(Some("[控制台]: ")),
            "prefix",
        )?
    } else {
        String::new()
    };
    let message = format!("{}{}", prefix, text);
    let receipt =
        required_enqueue_receipt(state.application.tasks.enqueue_console_chat(text, prefix))?;
    Ok(json!({
        "ok": true,
        "queued": true,
        "taskId": receipt.task_id,
        "position": receipt.position,
        "message": message
    })
    .to_string())
}

pub(super) fn required_enqueue_receipt(
    outcome: Result<FormalTaskEnqueueOutcome, impl std::fmt::Display>,
) -> std::result::Result<EnqueueReceipt, AppError> {
    let outcome = outcome.map_err(internal_error)?;
    match outcome {
        FormalTaskEnqueueOutcome::Queued(receipt) => Ok(EnqueueReceipt {
            task_id: receipt.task_id,
            position: receipt.position,
        }),
        FormalTaskEnqueueOutcome::Duplicate => Err(AppError {
            status: 409,
            message: "任务已在待执行范围内".to_string(),
        }),
    }
}
