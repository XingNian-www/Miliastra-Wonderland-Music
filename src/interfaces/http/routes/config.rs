use super::*;

use crate::config::{
    ConfigFieldError, ConfigSaveError, ConfigSaveOutcome, ConfigSectionSchema, ConfigSource,
    Effect, FieldKind, config_sections, default_config_json, default_config_json_with_audio_cache,
    section_schema,
};
use serde_json::{Map, Value};

/// 锁定配置存储句柄；锁损坏按内部错误处理。
fn lock_store(
    state: &HttpSharedState,
) -> std::result::Result<std::sync::MutexGuard<'_, crate::config::ConfigStore>, AppError> {
    state
        .config_store
        .lock()
        .map_err(|_| internal_error(anyhow!("配置存储锁已损坏")))
}

/// 保存/回滚成功落库后调用：重读完整配置并覆盖热更新共享值，
/// 使 schema 中标 Live 的字段立即作用于运行态消费方。
/// 由 [`save_outcome_response_after_apply`] 以尽力而为方式调用：
/// 失败只记录日志，不改变已提交的保存结果。
fn apply_live_configs(
    state: &HttpSharedState,
    store: &crate::config::ConfigStore,
) -> std::result::Result<(), AppError> {
    let config = store.load_full().map_err(internal_error)?;
    state.live_configs.apply(&config);
    Ok(())
}

/// 按点路径读取嵌套值；中间节点必须是对象，路径不存在时返回 None。
fn get_value_by_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = root;
    for segment in path.split('.') {
        current = current.as_object()?.get(segment)?;
    }
    Some(current)
}

/// FieldKind → 表单控件描述 JSON。
fn kind_json(kind: &FieldKind) -> Value {
    match kind {
        FieldKind::Bool => json!({ "type": "bool" }),
        FieldKind::Int { min, max } => json!({ "type": "int", "min": min, "max": max }),
        FieldKind::Float { min, max } => json!({ "type": "float", "min": min, "max": max }),
        FieldKind::String => json!({ "type": "string" }),
        FieldKind::Path => json!({ "type": "path" }),
        FieldKind::Enum(pairs) => json!({
            "type": "enum",
            "values": pairs
                .iter()
                .map(|(value, label)| json!({ "value": value, "label": label }))
                .collect::<Vec<_>>(),
        }),
        FieldKind::StringArray => json!({ "type": "stringArray" }),
        FieldKind::Object => json!({ "type": "object" }),
        FieldKind::Rect => json!({ "type": "rect" }),
        FieldKind::Point => json!({ "type": "point" }),
        FieldKind::Secret => json!({ "type": "secret" }),
    }
}

/// 来源小写字符串。
fn source_str(source: ConfigSource) -> &'static str {
    match source {
        ConfigSource::Db => "db",
        ConfigSource::Bootstrap => "bootstrap",
    }
}

/// 字段 schema JSON：default 从内置默认配置按点路径提取，提取不到为 null
/// （如 audio_cache 为 null 时其子字段 default 为 null）。
///
/// audio_cache.* 子字段改用 [`default_config_json_with_audio_cache`] 提取：
/// 默认 audio_cache=null 时子字段无默认值，Web「启用」按钮需要真实默认值预填。
fn field_json(
    field: &crate::config::ConfigFieldSchema,
    defaults: &Value,
    audio_cache_defaults: &Value,
) -> Value {
    let source_defaults = if field.path.contains(".audio_cache.") {
        audio_cache_defaults
    } else {
        defaults
    };
    json!({
        "path": field.path,
        "label": field.label,
        "kind": kind_json(&field.kind),
        "effect": field.effect,
        "source": source_str(field.source),
        "hint": field.hint,
        "nullable": field.nullable,
        "optionalParent": field.optional_parent,
        "default": get_value_by_path(source_defaults, &field.path)
            .cloned()
            .unwrap_or(Value::Null),
    })
}

/// 段 schema JSON（config_schema_route 使用，不含段当前值）。
fn section_json(
    section: &ConfigSectionSchema,
    defaults: &Value,
    audio_cache_defaults: &Value,
) -> Value {
    json!({
        "name": section.name,
        "label": section.label,
        "order": section.order,
        "fields": section
            .fields
            .iter()
            .map(|field| field_json(field, defaults, audio_cache_defaults))
            .collect::<Vec<_>>(),
    })
}

/// 字段级错误列表 → JSON 数组。
fn field_errors_json(errors: &[ConfigFieldError]) -> Vec<Value> {
    errors
        .iter()
        .map(|error| {
            json!({
                "section": error.section,
                "field": error.field,
                "message": error.message,
            })
        })
        .collect()
}

/// 保存/回滚落库成功后的收尾：先 best-effort 应用即时热更新（失败只记录日志，
/// 绝不把已提交的保存误报为失败），再按变更字段登记闲置重载并构造成功响应。
pub(super) fn save_outcome_response_after_apply(
    state: &HttpSharedState,
    store: &crate::config::ConfigStore,
    outcome: ConfigSaveOutcome,
    committed_label: &str,
) -> std::result::Result<String, AppError> {
    if let Err(error) = apply_live_configs(state, store) {
        log::error!("{}，但热更新应用失败: {}", committed_label, error.message);
    }
    let (restart_fields, reload_fields, applied_live_fields) =
        split_changed_fields_by_effect(&outcome.changed_fields);
    let reload_scheduled = !reload_fields.is_empty();
    if reload_scheduled {
        state
            .live_configs
            .schedule_reload(reload_fields.iter().cloned());
    }
    save_outcome_json(&outcome, restart_fields, reload_fields, applied_live_fields)
}

/// 校验失败业务响应（HTTP 200 + ok=false，前端按 ok 判断）。
fn validation_failed_json(errors: &[ConfigFieldError]) -> Value {
    json!({
        "ok": false,
        "code": "config_validation_failed",
        "message": "配置校验失败",
        "errors": field_errors_json(errors),
    })
}

/// 把变更字段按 schema 生效级别拆分为「人工重启」「闲置自动重载」与
/// 「已即时热更新」三组；
/// Object/Rect/Point 的 JSON 叶子路径继承其 schema 父字段效果；其余未声明路径
/// （如 state.playback_state_path 注入项）归入重启生效。
/// appliedLiveFields 只收 effect==Live 的字段（保存成功后已由
/// [`crate::config::LiveConfigs::apply`] 真正作用到运行态）。
fn split_changed_fields_by_effect(
    changed_fields: &[String],
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let fields = config_sections()
        .into_iter()
        .flat_map(|section| section.fields)
        .collect::<Vec<_>>();
    let mut restart = Vec::new();
    let mut idle_reload = Vec::new();
    let mut applied_live = Vec::new();
    for path in changed_fields {
        let effect = fields
            .iter()
            .filter(|field| {
                field.path == *path
                    || (matches!(
                        field.kind,
                        FieldKind::Object | FieldKind::Rect | FieldKind::Point
                    ) && path
                        .strip_prefix(&field.path)
                        .is_some_and(|suffix| suffix.starts_with('.')))
            })
            .max_by_key(|field| field.path.len())
            .map(|field| field.effect);
        match effect {
            Some(Effect::Live) => applied_live.push(path.clone()),
            Some(Effect::IdleReload) => idle_reload.push(path.clone()),
            Some(Effect::Restart) | None => restart.push(path.clone()),
        }
    }
    (restart, idle_reload, applied_live)
}

/// 保存/回滚成功响应；restartRequired 由本次变更中实际需要重启的字段决定，
/// reloadScheduled 表示本次变更已登记为闲置时自动重载。
fn save_outcome_json(
    outcome: &ConfigSaveOutcome,
    restart_fields: Vec<String>,
    reload_fields: Vec<String>,
    applied_live_fields: Vec<String>,
) -> std::result::Result<String, AppError> {
    let reload_scheduled = !reload_fields.is_empty();
    serde_json::to_string(&json!({
        "ok": true,
        "revision": outcome.revision,
        "changedFields": outcome.changed_fields,
        "restartRequired": !restart_fields.is_empty(),
        "restartFields": restart_fields,
        "reloadScheduled": reload_scheduled,
        "reloadFields": reload_fields,
        "appliedLiveFields": applied_live_fields,
    }))
    .map_err(internal_error)
}

/// 请求体必须包含 sections 对象，且每个段值为对象（与 ConfigStore::save 的
/// 整段替换语义一致）；缺失或形状不对按业务错误（400）拒绝。
fn parse_sections(body: &Value) -> std::result::Result<&Map<String, Value>, AppError> {
    let sections = body
        .get("sections")
        .and_then(Value::as_object)
        .ok_or_else(|| bad_request("请求必须包含 sections 对象"))?;
    if sections.values().any(|value| !value.is_object()) {
        return Err(bad_request("sections 的每个段值必须是对象"));
    }
    Ok(sections)
}

/// GET /config：完整配置（脱敏后，路径保持库中相对值）+ 当前版本号 + schema 版本。
pub(super) fn config_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let store = lock_store(state)?;
    // current_value 不解析相对路径：避免 Web 表单回填绝对路径后保存回写。
    let sections = store.current_value().map_err(internal_error)?;
    let revision = store.current_revision().map_err(internal_error)?;
    serde_json::to_string(&json!({
        "ok": true,
        "revision": revision,
        "schemaVersion": crate::config::CONFIG_SCHEMA_VERSION,
        "sections": sections,
    }))
    .map_err(internal_error)
}

/// GET /config/schema：全部段与字段 schema（含内置默认值）。
pub(super) fn config_schema_route(
    _query: &[(String, String)],
    _state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let defaults = default_config_json();
    let audio_cache_defaults = default_config_json_with_audio_cache();
    let sections = config_sections()
        .iter()
        .map(|section| section_json(section, &defaults, &audio_cache_defaults))
        .collect::<Vec<_>>();
    serde_json::to_string(&json!({ "ok": true, "sections": sections })).map_err(internal_error)
}

/// GET /config/section?name=段名：单段 schema + 脱敏当前值 + 版本号。
pub(super) fn config_section_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let name = query_value(query, "name").unwrap_or_default();
    let Some(section) = section_schema(name) else {
        return Err(bad_request(&format!("配置段不存在: {name}")));
    };
    let store = lock_store(state)?;
    let revision = store.current_revision().map_err(internal_error)?;
    // 先对完整配置根对象脱敏（点路径含段前缀，如 ai.api_key），再提取目标段；
    // 若先提取段对象再脱敏，路径前缀被截断（api_key），SECRET_PATHS 匹配失败，
    // 单段接口会明文泄漏 http.access_token、ai.api_key 等密钥。
    // 不解析相对路径（current_value）：与 GET /config 一致，避免绝对路径回写。
    let sections = store.current_value().map_err(internal_error)?;
    let values = sections.get(name).cloned().unwrap_or(Value::Null);
    let defaults = default_config_json();
    let audio_cache_defaults = default_config_json_with_audio_cache();
    serde_json::to_string(&json!({
        "ok": true,
        "name": section.name,
        "label": section.label,
        "order": section.order,
        "fields": section
            .fields
            .iter()
            .map(|field| field_json(field, &defaults, &audio_cache_defaults))
            .collect::<Vec<_>>(),
        "values": values,
        "revision": revision,
    }))
    .map_err(internal_error)
}

/// GET /config/revisions：历史版本列表（按 revision 倒序）。
pub(super) fn config_revisions_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let store = lock_store(state)?;
    let revisions = store
        .revisions()
        .map_err(internal_error)?
        .into_iter()
        .map(|info| json!({ "revision": info.revision, "createdAtMs": info.created_at_ms }))
        .collect::<Vec<_>>();
    serde_json::to_string(&json!({ "ok": true, "revisions": revisions })).map_err(internal_error)
}

/// POST /config/validate：候选配置预检，errors 为空表示有效。
pub(super) fn config_validate_route(
    body: &[u8],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let body: Value = parse_json_body(body, "配置校验请求")?;
    let sections = parse_sections(&body)?;
    let store = lock_store(state)?;
    let errors = store.validate_candidate(sections).map_err(internal_error)?;
    serde_json::to_string(&json!({
        "ok": true,
        "errors": field_errors_json(&errors),
    }))
    .map_err(internal_error)
}

/// POST /config/save：带基线版本号的整段替换保存。
pub(super) fn config_save_route(
    body: &[u8],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let body: Value = parse_json_body(body, "配置保存请求")?;
    let base_revision = body
        .get("baseRevision")
        .and_then(Value::as_u64)
        .ok_or_else(|| bad_request("请求必须包含 baseRevision 整数"))?;
    let sections = parse_sections(&body)?.clone();
    let mut store = lock_store(state)?;
    // 预检：字段错误直接返回（与 save 同一校验路径），不落库。
    let errors = store
        .validate_candidate(&sections)
        .map_err(internal_error)?;
    if !errors.is_empty() {
        return serde_json::to_string(&validation_failed_json(&errors)).map_err(internal_error);
    }
    match store.save(base_revision, sections) {
        Ok(outcome) => {
            // 保存已成功提交（库已变更）：热更新应用改为尽力而为，
            // 失败只记录日志，仍返回保存成功响应，避免客户端误判保存失败。
            save_outcome_response_after_apply(state, &store, outcome, "配置已保存")
        }
        // 版本冲突：事务内重读版本与基线不一致（并发修改）。
        Err(ConfigSaveError::Conflict) => serde_json::to_string(&json!({
            "ok": false,
            "code": "config_conflict",
            "message": "配置已被其他修改，请刷新后重试",
            "errors": [],
        }))
        .map_err(internal_error),
        Err(error) => Err(internal_error(error)),
    }
}

/// POST /config/rollback：回滚到指定历史版本（记录为新版本）。
pub(super) fn config_rollback_route(
    body: &[u8],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let body: Value = parse_json_body(body, "配置回滚请求")?;
    let revision = body
        .get("revision")
        .and_then(Value::as_u64)
        .ok_or_else(|| bad_request("请求必须包含 revision 整数"))?;
    let base_revision = body
        .get("baseRevision")
        .and_then(Value::as_u64)
        .ok_or_else(|| bad_request("请求必须包含 baseRevision 整数"))?;
    let mut store = lock_store(state)?;
    match store.rollback(revision, base_revision) {
        Ok(outcome) => {
            // 回滚同样是完整配置替换：落库已提交后热更新应用改为尽力而为，
            // 失败只记录日志，仍返回回滚成功响应。
            save_outcome_response_after_apply(state, &store, outcome, "配置已回滚")
        }
        Err(ConfigSaveError::RevisionNotFound(_)) => serde_json::to_string(&json!({
            "ok": false,
            "code": "config_revision_not_found",
            "message": format!("目标版本 {revision} 不存在"),
            "errors": [],
        }))
        .map_err(internal_error),
        Err(ConfigSaveError::Conflict) => serde_json::to_string(&json!({
            "ok": false,
            "code": "config_conflict",
            "message": "配置已被其他修改，请刷新后重试",
            "errors": [],
        }))
        .map_err(internal_error),
        Err(error) => Err(internal_error(error)),
    }
}
