use super::*;

pub(super) fn turtle_soup_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    serde_json::to_string(
        &state
            .application
            .queries
            .turtle_soup_snapshot()
            .map_err(internal_error)?,
    )
    .map_err(internal_error)
}

pub(super) fn turtle_soup_start_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let puzzle_id = query_value(query, "id")
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string);
    let BusinessMutationOutcome::TurtleSoup(outcome) = state
        .application
        .tasks
        .apply_mutation(BusinessMutationIntent::TurtleSoup(
            TurtleSoupMutationIntent::Start { puzzle_id },
        ))
        .map_err(internal_error)?
    else {
        unreachable!("turtle soup start intent returned a different outcome")
    };
    let TurtleSoupMutationOutcome::Started(snapshot) = *outcome else {
        unreachable!("turtle soup start intent returned a different outcome")
    };
    serde_json::to_string(&json!({
        "ok": true,
        "turtleSoup": snapshot,
    }))
    .map_err(internal_error)
}

pub(super) fn turtle_soup_end_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let BusinessMutationOutcome::TurtleSoup(outcome) = state
        .application
        .tasks
        .apply_mutation(BusinessMutationIntent::TurtleSoup(
            TurtleSoupMutationIntent::End,
        ))
        .map_err(internal_error)?
    else {
        unreachable!("turtle soup end intent returned a different outcome")
    };
    let TurtleSoupMutationOutcome::Ended { ended, snapshot } = *outcome else {
        unreachable!("turtle soup end intent returned a different outcome")
    };
    if !ended {
        return Err(AppError {
            status: 409,
            message: "当前没有可结束的海龟汤".to_string(),
        });
    }
    serde_json::to_string(&json!({
        "ok": true,
        "turtleSoup": snapshot,
    }))
    .map_err(internal_error)
}

pub(super) fn turtle_soup_questions_route(
    body: &[u8],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let submission =
        serde_json::from_slice::<TurtleSoupSubmission>(body).map_err(|error| AppError {
            status: 400,
            message: format!("海龟汤提交JSON无效: {error}"),
        })?;
    if submission.title.trim().is_empty()
        || submission.surface.trim().is_empty()
        || submission.bottom.trim().is_empty()
    {
        return Err(bad_request("海龟汤标题、汤面和汤底不能为空"));
    }
    let BusinessMutationOutcome::TurtleSoup(outcome) = state
        .application
        .tasks
        .apply_mutation(BusinessMutationIntent::TurtleSoup(
            TurtleSoupMutationIntent::AppendPuzzle(submission),
        ))
        .map_err(internal_error)?
    else {
        unreachable!("turtle soup append intent returned a different outcome")
    };
    let TurtleSoupMutationOutcome::PuzzleAppended(receipt) = *outcome else {
        unreachable!("turtle soup append intent returned a different outcome")
    };
    serde_json::to_string(&receipt).map_err(internal_error)
}

pub(super) fn undercover_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let snapshot = state
        .application
        .queries
        .undercover_snapshot()
        .map_err(internal_error)?;
    serde_json::to_string(&snapshot).map_err(internal_error)
}

pub(super) fn undercover_start_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    state
        .application
        .commands
        .undercover_control(true)
        .map_err(command_error)
}

pub(super) fn undercover_end_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    state
        .application
        .commands
        .undercover_control(false)
        .map_err(command_error)
}
