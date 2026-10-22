use super::*;
use crate::features::command_access::{
    CommandAccessConfig, CommandPermission, CommandPolicy, catalog_policy,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

use crate::config::StoredReply;

const MAX_COMMAND_REPLIES: usize = 32;
const MAX_REPLY_CHARS: usize = 300;

/// 命令目录与可编辑策略共享稳定ID，读取本身不改变配置。
pub(super) fn commands_replies_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let store = state
        .config_store
        .lock()
        .map_err(|_| internal_error(anyhow!("配置存储锁已损坏")))?;
    let revision = store.reply_revision().map_err(internal_error)?;
    let stored = store.reply_templates().map_err(internal_error)?;
    let commands = crate::features::reply_template::command_reply_catalog().iter().map(|entry| {
        let defaults = entry.replies.iter().map(|item| json!({"key":item.key,"text":item.default,"defaultText":item.default,"editable":true,"active":true})).collect::<Vec<_>>();
        let stored_rows = stored.get(entry.id).map(Vec::as_slice);
        let default_by_key: BTreeMap<_, _> = entry.replies.iter().map(|item| (item.key, item.default)).collect();
        let replies = match stored_rows {
            Some(rows) if rows.is_empty() => entry.replies.iter().map(|item| json!({
                "key": item.key, "text": item.default, "defaultText": item.default, "editable": true, "active": false
            })).collect::<Vec<_>>(),
            Some(rows) => {
                let active: BTreeSet<_> = rows.iter().map(|row| row.key.as_str()).collect();
                let mut replies = rows.iter().map(|row| json!({
                    "key":row.key,
                    "text":row.text,
                    "defaultText":default_by_key.get(row.key.as_str()).copied(),
                    "editable":true,
                    "active":true
                })).collect::<Vec<_>>();
                replies.extend(entry.replies.iter().filter(|item| !active.contains(item.key))
                    .map(|item| json!({"key":item.key,"text":item.default,"defaultText":item.default,"editable":true,"active":false})));
                replies
            },
            None => entry.replies.iter().map(|item| json!({
                "key": item.key, "text": item.default, "defaultText": item.default, "editable": true, "active": true
            })).collect::<Vec<_>>(),
        };
        let mut referenced_variables = BTreeSet::new();
        for item in entry.replies {
            let mut rest = item.default;
            while let Some(open) = rest.find('{') {
                let after = &rest[open + 1..];
                let Some(close) = after.find('}') else { break };
                let name = &after[..close];
                if crate::features::reply_template::reply_variable_allowed(name) {
                    referenced_variables.insert(name);
                }
                rest = &after[close + 1..];
            }
        }
        let allowed_variables = crate::features::reply_template::reply_variables().iter()
            .filter(|variable| referenced_variables.contains(variable.name))
            .map(|variable| variable.name).collect::<Vec<_>>();
        json!({ "id": entry.id, "name": entry.name, "category": entry.category, "orderedBatch": entry.ordered_batch, "replies": replies, "defaultReplies": defaults, "allowedVariables": allowed_variables })
    }).collect::<Vec<_>>();
    let variables = crate::features::reply_template::reply_variables()
        .iter()
        .map(|variable| json!({"name":variable.name,"label":variable.label}))
        .collect::<Vec<_>>();
    serde_json::to_string(&json!({"revision":revision,"commands":commands,"variables":variables}))
        .map_err(internal_error)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RepliesSaveRequest {
    base_revision: u64,
    commands: Vec<RepliesCommand>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RepliesCommand {
    id: String,
    replies: Vec<RepliesItem>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RepliesItem {
    key: String,
    text: String,
}

fn validate_reply_items(
    entry: &crate::features::reply_template::CommandReplyCatalogEntry,
    items: Vec<RepliesItem>,
) -> std::result::Result<Vec<StoredReply>, AppError> {
    if items.len() > MAX_COMMAND_REPLIES {
        return Err(bad_request("每个命令最多32条回复"));
    }
    let static_keys: BTreeSet<_> = entry.replies.iter().map(|item| item.key).collect();
    let mut seen = BTreeSet::new();
    let mut saved = Vec::with_capacity(items.len());
    for reply in items {
        if reply.key.trim().is_empty() || !seen.insert(reply.key.clone()) {
            return Err(bad_request("回复key不能为空且不能重复"));
        }
        if reply.text.trim().is_empty() {
            return Err(bad_request("活动回复内容不能为空；请停用该行或静音命令"));
        }
        if reply.text.chars().count() > MAX_REPLY_CHARS
            || reply.text.contains('\n')
            || reply.text.contains('\r')
        {
            return Err(bad_request("单条回复最多300字且必须为单行"));
        }
        if !static_keys.contains(reply.key.as_str())
            && (!entry.ordered_batch
                || !reply
                    .key
                    .strip_prefix("custom:")
                    .is_some_and(|suffix| !suffix.trim().is_empty()))
        {
            return Err(bad_request(
                "静态回复key不属于该命令，自定义key必须使用custom:前缀",
            ));
        }
        if !reply_variables_valid(&reply.text) {
            return Err(bad_request("回复包含未知变量占位符"));
        }
        saved.push(StoredReply {
            key: reply.key,
            text: reply.text,
        });
    }
    Ok(saved)
}

pub(super) fn commands_replies_save_route(
    body: &[u8],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let request: RepliesSaveRequest = parse_json_body(body, "命令回复保存请求")?;
    if request.commands.is_empty() || request.commands.len() > 64 {
        return Err(bad_request("请提交1至64个命令的回复列表"));
    }
    let catalog = crate::features::reply_template::command_reply_catalog();
    let mut replacements = BTreeMap::<String, Vec<StoredReply>>::new();
    for command in request.commands {
        let entry = catalog
            .iter()
            .find(|entry| entry.id == command.id)
            .ok_or_else(|| bad_request("命令ID不在可编辑回复目录中"))?;
        if replacements.contains_key(&command.id) {
            return Err(bad_request("命令ID重复"));
        }
        replacements.insert(
            command.id.clone(),
            validate_reply_items(entry, command.replies)?,
        );
    }
    let mut store = state
        .config_store
        .lock()
        .map_err(|_| internal_error(anyhow!("配置存储锁已损坏")))?;
    match store.save_reply_templates(request.base_revision, &replacements) {
        Ok(revision) => serde_json::to_string(&json!({"ok":true,"revision":revision})).map_err(internal_error),
        Err(error) if error.to_string().contains("revision 冲突") => serde_json::to_string(
            &json!({"ok":false,"code":"reply_conflict","message":"回复配置已变化，请重新加载后保存"})
        ).map_err(internal_error),
        Err(error) => Err(bad_request(&error.to_string())),
    }
}

fn reply_variables_valid(text: &str) -> bool {
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            return true;
        };
        let name = &after[..close];
        if !name.is_empty()
            && !name.contains('{')
            && !crate::features::reply_template::reply_variable_allowed(name)
        {
            return false;
        }
        rest = &after[close + 1..];
    }
    true
}

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
    view["defaultSongSource"] = json!(
        config
            .command_access
            .default_song_source
            .as_deref()
            .unwrap_or("qqmusic")
    );
    view["songPlatforms"] = json!(
        crate::features::song_request::platform_sources()
            .map(|(source, label)| json!({"value":source.as_str(),"label":label}))
            .collect::<Vec<_>>()
    );
    let defaults = CommandAccessConfig::default();
    if let Some(groups) = view["groups"].as_array_mut() {
        for group in groups {
            if let Some(commands) = group["commands"].as_array_mut() {
                for command in commands {
                    let name = command["name"].as_str().unwrap_or("");
                    let prefix = command["prefix"].as_str().unwrap_or("");
                    let id = if prefix == "@" {
                        crate::features::song_request::permission_id_for_command_name(name)
                            .map(str::to_owned)
                            .unwrap_or_else(|| format!("@{name}"))
                    } else {
                        format!("{prefix}{name}")
                    };
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
    #[serde(default)]
    items: Vec<PolicyItem>,
    default_song_source: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PolicyItem {
    id: String,
    enabled: bool,
    minimum_role: CommandPermission,
}

#[cfg(test)]
mod reply_api_tests {
    use super::*;

    #[test]
    fn reply_variables_accept_only_registered_runtime_names() {
        assert!(reply_variables_valid("正在播放{歌名}，请求者{用户名}"));
        assert!(reply_variables_valid("花括号{}保留"));
        assert!(reply_variables_valid("前缀{未闭合"));
        assert!(!reply_variables_valid("未知{HTTP内部字段}"));
    }

    fn help_song_entry() -> &'static crate::features::reply_template::CommandReplyCatalogEntry {
        crate::features::reply_template::command_reply_catalog()
            .iter()
            .find(|entry| entry.id == "@帮助")
            .expect("help reply catalog entry")
    }

    fn item(key: &str, text: String) -> RepliesItem {
        RepliesItem {
            key: key.to_owned(),
            text,
        }
    }

    fn single_reply_entry() -> &'static crate::features::reply_template::CommandReplyCatalogEntry {
        crate::features::reply_template::command_reply_catalog()
            .iter()
            .find(|entry| entry.id == "@音量")
            .expect("single reply catalog entry")
    }

    #[test]
    fn reply_item_validation_accepts_ordered_static_and_custom_rows() {
        let rows = validate_reply_items(
            help_song_entry(),
            vec![
                item("custom:first", "第一条{用户名}".to_owned()),
                item("help.song_request.1", "第二条".to_owned()),
            ],
        )
        .unwrap();
        assert_eq!(
            rows.iter().map(|row| row.key.as_str()).collect::<Vec<_>>(),
            ["custom:first", "help.song_request.1"]
        );
    }

    #[test]
    fn branch_outcome_rejects_extra_reply_keys() {
        assert!(
            validate_reply_items(
                single_reply_entry(),
                vec![item("custom:extra", "不应添加第二个分支回复".to_owned())],
            )
            .is_err()
        );
        assert!(
            validate_reply_items(
                single_reply_entry(),
                vec![item("playback.volume.set", "音量 {音量}".to_owned())],
            )
            .is_ok()
        );
    }

    #[test]
    fn reply_item_validation_rejects_blank_oversized_multiline_and_unknown_variables() {
        for text in [String::new(), "  ".to_owned()] {
            assert!(
                validate_reply_items(help_song_entry(), vec![item("help.song_request.1", text)])
                    .is_err()
            );
        }
        assert!(
            validate_reply_items(
                help_song_entry(),
                vec![item(
                    "help.song_request.1",
                    "歌".repeat(MAX_REPLY_CHARS + 1)
                )],
            )
            .is_err()
        );
        assert!(
            validate_reply_items(
                help_song_entry(),
                vec![item("help.song_request.1", "第一行\n第二行".to_owned())],
            )
            .is_err()
        );
        assert!(
            validate_reply_items(
                help_song_entry(),
                vec![item("help.song_request.1", "未知{HTTP内部字段}".to_owned())],
            )
            .is_err()
        );
    }

    #[test]
    fn empty_reply_list_is_valid_for_explicit_mute() {
        assert!(
            validate_reply_items(help_song_entry(), Vec::new())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn reply_endpoints_are_registered_with_expected_methods() {
        assert!(route_spec("/commands/replies").is_some_and(|route| !route.mutating));
        assert!(body_route_spec("/commands/replies").is_some());
    }
}

pub(super) fn commands_policy_save_route(
    body: &[u8],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let request: PolicySaveRequest = parse_json_body(body, "命令权限保存请求")?;
    if (request.items.is_empty() && request.default_song_source.is_none())
        || request.items.len() > 256
    {
        return Err(bad_request("请提交1至256条命令策略"));
    }
    let mut store = state
        .config_store
        .lock()
        .map_err(|_| internal_error(anyhow!("配置存储锁已损坏")))?;
    let mut config = store.load_full().map_err(internal_error)?.command_access;
    if let Some(source) = request.default_song_source {
        config.default_song_source = Some(source);
    }
    let mut seen = std::collections::BTreeSet::new();
    for item in request.items {
        if matches!(item.id.as_str(), "@点歌" | "@搜索")
            || !seen.insert(item.id.clone())
            || catalog_policy(&config, &item.id).is_none()
        {
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
