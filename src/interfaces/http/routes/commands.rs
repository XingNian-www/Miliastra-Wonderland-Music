use super::*;
use crate::features::command_access::{
    CommandAccessConfig, CommandPermission, CommandPolicy, catalog_policy,
};
use serde_json::{Value, json};

/// 命令目录与可编辑策略共享稳定ID，读取本身不改变配置。
pub(super) fn commands_catalog_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let enabled = state.monitor.snapshot().operational.commands_enabled;
    let store = state
        .config_store
        .lock()
        .map_err(|_| internal_error(anyhow!("配置存储锁已损坏")))?;
    let config = store.load_full().map_err(internal_error)?;
    let mut view = serde_json::to_value(crate::features::command_catalog::catalog_view(enabled))
        .map_err(internal_error)?;
    view["revision"] = json!(store.current_revision().map_err(internal_error)?);
    let defaults = CommandAccessConfig::default();
    if let Some(groups) = view["groups"].as_array_mut() {
        for group in groups {
            if let Some(commands) = group["commands"].as_array_mut() {
                for command in commands {
                    let id = format!(
                        "{}{}",
                        command["prefix"].as_str().unwrap_or(""),
                        command["name"].as_str().unwrap_or("")
                    );
                    command["id"] = json!(id);
                    if let Some(policy) = catalog_policy(&config.command_access, &id) {
                        command["enabled"] = json!(policy.enabled);
                        command["minimumRole"] = json!(policy.minimum_role);
                        command["defaultMinimumRole"] =
                            json!(catalog_policy(&defaults, &id).map(|p| p.minimum_role));
                        command["editable"] = json!(true);
                        let floor = crate::features::command_access::minimum_floor(&id);
                        command["minimumFloor"] = json!(floor);
                        command["lockedReason"] = if floor == CommandPermission::Owner {
                            json!("此全局控制命令最低需要主人权限，可单独禁用")
                        } else {
                            Value::Null
                        };
                    } else {
                        command["editable"] = json!(false);
                        command["lockedReason"] =
                            json!("该条目描述会话输入，请修改对应玩法命令的权限");
                    }
                }
            }
        }
    }
    // 自定义流程共用可编辑权限入口，不允许以来源颜色绕过。
    let workflow = catalog_policy(&config.command_access, "custom_workflow").unwrap();
    if let Some(groups) = view["groups"].as_array_mut() {
        groups.push(json!({"name":"自定义流程","commands":[{
            "id":"custom_workflow","name":"全部自定义流程","prefix":"","aliases":[],"usage":[],"examples":[],"authorities":[],"openEnded":false,
            "summary":"适用于所有已配置自定义流程，仍保留各流程自身来源及确认规则",
            "enabled":workflow.enabled,"minimumRole":workflow.minimum_role,"defaultMinimumRole":"admin","minimumFloor":"user","editable":true,"lockedReason":null
        }]}));
    }
    serde_json::to_string(&view).map_err(internal_error)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PolicySaveRequest {
    base_revision: u64,
    items: Vec<PolicyItem>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PolicyItem {
    id: String,
    enabled: bool,
    minimum_role: CommandPermission,
}

pub(super) fn commands_policy_save_route(
    body: &[u8],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let request: PolicySaveRequest = parse_json_body(body, "命令权限保存请求")?;
    if request.items.is_empty() || request.items.len() > 256 {
        return Err(bad_request("请提交1至256条命令策略"));
    }
    let mut store = state
        .config_store
        .lock()
        .map_err(|_| internal_error(anyhow!("配置存储锁已损坏")))?;
    let mut config = store.load_full().map_err(internal_error)?.command_access;
    let mut seen = std::collections::BTreeSet::new();
    for item in request.items {
        if !seen.insert(item.id.clone()) || catalog_policy(&config, &item.id).is_none() {
            return Err(bad_request("命令ID重复或不在可编辑目录中"));
        }
        config.overrides.insert(
            item.id,
            CommandPolicy {
                enabled: item.enabled,
                minimum_role: item.minimum_role,
            },
        );
    }
    config
        .validate()
        .map_err(|error| bad_request(&error.to_string()))?;
    let sections =
        serde_json::from_value(json!({"command_access": config})).map_err(internal_error)?;
    match store.save(request.base_revision, sections) {
        Ok(outcome) => save_outcome_response_after_apply(state, &store, outcome, "命令权限已保存"),
        Err(crate::config::ConfigSaveError::Conflict) => serde_json::to_string(
            &json!({"ok":false,"code":"config_conflict","message":"配置已变化，请重新加载后保存"}),
        )
        .map_err(internal_error),
        Err(error) => Err(internal_error(error)),
    }
}
