use super::*;
use crate::features::command_access::{
    CommandAccessConfig, CommandPermission, CommandPolicy, catalog_policy,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

use crate::config::StoredReply;

const MAX_COMMAND_REPLIES: usize = 32;
/// 单条回复的字数上限；与配置页的 replies.overrides 共用同一个来源，
/// 避免两个编辑入口对同一条文案给出不同限制。
const MAX_REPLY_CHARS: usize = crate::config::MAX_REPLY_OVERRIDE_CHARS;

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
    let commands = crate::features::reply_template::command_reply_catalog().map(|entry| {
        let owned: Vec<_> = entry.replies().collect();
        let defaults = owned.iter().map(|item| json!({"key":item.key,"text":item.default,"defaultText":item.default,"editable":true,"active":true})).collect::<Vec<_>>();
        let stored_rows = stored.get(entry.id).map(Vec::as_slice);
        let default_by_key: BTreeMap<_, _> = owned.iter().map(|item| (item.key, item.default)).collect();
        let replies = match stored_rows {
            Some(rows) if rows.is_empty() => owned.iter().map(|item| json!({
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
                replies.extend(owned.iter().filter(|item| !active.contains(item.key))
                    .map(|item| json!({"key":item.key,"text":item.default,"defaultText":item.default,"editable":true,"active":false})));
                replies
            },
            None => owned.iter().map(|item| json!({
                "key": item.key, "text": item.default, "defaultText": item.default, "editable": true, "active": true
            })).collect::<Vec<_>>(),
        };
        let mut referenced_variables = BTreeSet::new();
        for item in &owned {
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
        let mut allowed_variables = crate::features::reply_template::reply_variables().iter()
            .filter(|variable| referenced_variables.contains(variable.name))
            .map(|variable| variable.name).collect::<Vec<_>>();
        // 全局标签与命令无关：任何回复都能用，因此总是放行。
        allowed_variables.extend(
            crate::features::global_tags::GLOBAL_TAGS
                .iter()
                .map(|tag| tag.name),
        );
        allowed_variables.push(crate::features::global_tags::AI_TAG);
        json!({ "id": entry.id, "name": entry.name, "category": entry.category, "replies": replies, "defaultReplies": defaults, "allowedVariables": allowed_variables })
    }).collect::<Vec<_>>();
    let mut variables = crate::features::reply_template::reply_variables()
        .iter()
        .map(|variable| {
            json!({
                "name": variable.name,
                "label": variable.label,
                "example": variable.example,
            })
        })
        .collect::<Vec<_>>();
    // 全局标签排在最后：它们不依赖命令上下文，示例取候选里第一条。
    variables.extend(crate::features::global_tags::GLOBAL_TAGS.iter().map(|tag| {
        json!({
            "name": tag.name,
            "label": tag.label,
            "example": tag.choices.first().copied().unwrap_or(""),
            "global": true,
        })
    }));
    // AI 标签走另一条求值路径（渲染前预生成），但同样要能让用户插入。
    variables.push(json!({
        "name": crate::features::global_tags::AI_TAG,
        "label": crate::features::global_tags::AI_TAG_LABEL,
        "example": crate::features::global_tags::AI_TAG_EXAMPLE,
        "global": true,
    }));
    serde_json::to_string(&json!({
        "revision": revision,
        "commands": commands,
        "variables": variables,
        "limits": { "maxReplyChars": MAX_REPLY_CHARS, "maxRows": MAX_COMMAND_REPLIES },
        // 共享回复的命令别名：前端据此把 @播放 这类命令指向它真正所在的回复分组。
        "aliases": crate::features::reply_template::command_id_aliases()
            .iter()
            .map(|(alias, owner)| json!({"id": alias, "owner": owner}))
            .collect::<Vec<_>>(),
    }))
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
    let static_keys: BTreeSet<_> = entry.replies().map(|item| item.key).collect();
    let mut seen = BTreeSet::new();
    let mut saved = Vec::with_capacity(items.len());
    for reply in items {
        if reply.key.trim().is_empty() || !seen.insert(reply.key.clone()) {
            return Err(bad_request("回复key不能为空且不能重复"));
        }
        // 留空 = 使用内置默认，但**仍要保留这一行**。
        //
        // 关键区别：行的存在表示"该回复启用"，行的缺失才表示"该回复被停用"。
        // 如果这里把空文本行丢弃，一条只含单行的命令保存后就会变成空列表，
        // 被 is_muted 判成"整条命令静音"——用户只是想恢复默认文案，却把回复关掉了。
        if reply.text.trim().is_empty() {
            saved.push(StoredReply {
                key: reply.key,
                text: String::new(),
            });
            continue;
        }
        // 允许换行：一行发一条消息，用户自己决定发几条。
        // 仍限制总字符数，避免一次粘贴成百上千字把聊天区刷爆。
        if reply.text.chars().count() > MAX_REPLY_CHARS {
            return Err(bad_request("单条回复最多300字"));
        }
        if reply.text.contains('\r') {
            return Err(bad_request("回复文案不能包含回车符，请使用换行"));
        }
        // 静态 key 必须属于该命令；custom: 前缀的自定义行始终允许。
        if !static_keys.contains(reply.key.as_str())
            && !reply
                .key
                .strip_prefix("custom:")
                .is_some_and(|suffix| !suffix.trim().is_empty())
        {
            return Err(bad_request(
                "静态回复key不属于该命令，自定义key必须使用custom:前缀",
            ));
        }
        validate_reply_variables(&reply.text).map_err(|reason| bad_request(&reason))?;
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
    let catalog: Vec<_> = crate::features::reply_template::command_reply_catalog().collect();
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

/// 校验回复文案里的变量占位符。
///
/// 语法是**单花括号** `{变量名}`，与 AI 聊天提示词那套 `{{双花括号}}` 不同。
///
/// 两种误用都必须报错，否则用户以为生效、实际原样发到聊天框：
///
/// - `{{玩家}}`：双花括号会被当成普通文本原样输出。原来这里也放过了它，
///   用户看不到任何提示，聊天里就出现字面量 `{{玩家}}`。
/// - `{昵称}`：表里没有的名字。原来只回一句「包含未知变量占位符」，
///   不说是哪个、也不说能用哪些，用户只能猜。
fn validate_reply_variables(text: &str) -> std::result::Result<(), String> {
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            // 未闭合的左花括号：渲染器同样原样保留，不算错误。
            return Ok(());
        };
        let name = &after[..close];
        if name.contains('{') {
            // `{{名字}}` —— 用户很可能把 AI 提示词的双花括号语法带过来了。
            let inner = name.trim_start_matches('{').trim();
            let inner = inner.trim_end_matches('}').trim();
            if !inner.is_empty() && inner.chars().all(|c| c != '{') {
                return Err(format!(
                    "回复里的 {{{{}}}} 不会被替换，请改用单花括号 {{{inner}}}"
                ));
            }
            rest = &after[close + 1..];
            continue;
        }
        if name.trim().is_empty() {
            rest = &after[close + 1..];
            continue;
        }
        if name.trim() != name {
            return Err(format!(
                "变量花括号内不能有空格，请写成 {{{}}}",
                name.trim()
            ));
        }
        if !crate::features::reply_template::reply_variable_allowed(name) {
            return Err(format!("未知变量 {{{name}}}。可用变量见回复框下方的按钮"));
        }
        rest = &after[close + 1..];
    }
    Ok(())
}

fn reply_variables_valid(text: &str) -> bool {
    validate_reply_variables(text).is_ok()
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
            "enabled":workflow.enabled,"minimumRole":workflow.minimum_role,"minimumFloor":"user","editable":true,"lockedReason":null
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
        assert!(reply_variables_valid("正在播放{歌名}，请求者{玩家}"));
        assert!(reply_variables_valid("花括号{}保留"));
        assert!(reply_variables_valid("前缀{未闭合"));
        assert!(!reply_variables_valid("未知{HTTP内部字段}"));
    }

    fn help_song_entry() -> crate::features::reply_template::CommandReplyCatalogEntry {
        crate::features::reply_template::command_reply_catalog()
            .find(|entry| entry.id == "@帮助")
            .expect("help reply catalog entry")
    }

    fn item(key: &str, text: String) -> RepliesItem {
        RepliesItem {
            key: key.to_owned(),
            text,
        }
    }

    fn single_reply_entry() -> crate::features::reply_template::CommandReplyCatalogEntry {
        crate::features::reply_template::command_reply_catalog()
            .find(|entry| entry.id == "@音量")
            .expect("single reply catalog entry")
    }

    #[test]
    fn reply_item_validation_accepts_static_and_custom_rows() {
        // 帮助已合并成单条 help.song_request；自定义行仍以 custom: 前缀表达。
        let rows = validate_reply_items(
            &help_song_entry(),
            vec![
                item("custom:first", "第一条{玩家}".to_owned()),
                item("help.song_request", "第二条".to_owned()),
            ],
        )
        .unwrap();
        assert_eq!(
            rows.iter().map(|row| row.key.as_str()).collect::<Vec<_>>(),
            ["custom:first", "help.song_request"]
        );
    }

    #[test]
    fn branch_outcome_rejects_unknown_static_keys_but_allows_custom_rows() {
        // custom: 行始终允许；不属于该命令的静态 key 必须拒绝。
        assert!(
            validate_reply_items(
                &single_reply_entry(),
                vec![item("custom:extra", "自定义行".to_owned())],
            )
            .is_ok()
        );
        assert!(
            validate_reply_items(
                &single_reply_entry(),
                vec![item("playback.pause.ok", "别的命令的 key".to_owned())],
            )
            .is_err()
        );
        assert!(
            validate_reply_items(
                &single_reply_entry(),
                vec![item("playback.volume.set", "音量 {音量}".to_owned())],
            )
            .is_ok()
        );
    }

    #[test]
    fn reply_item_validation_rejects_blank_oversized_multiline_and_unknown_variables() {
        // 留空 = 使用内置默认，但行必须保留：行存在才表示该回复启用。
        // 若把行丢掉，只含单行的命令保存后就会变成空列表而被判为“整条静音”。
        for text in [String::new(), "  ".to_owned()] {
            let rows =
                validate_reply_items(&help_song_entry(), vec![item("help.song_request", text)])
                    .expect("留空应被接受");
            assert_eq!(rows.len(), 1, "留空的行必须保留");
            assert!(rows[0].text.trim().is_empty(), "留空行的文本应为空");
        }
        assert!(
            validate_reply_items(
                &help_song_entry(),
                vec![item("help.song_request", "歌".repeat(MAX_REPLY_CHARS + 1))],
            )
            .is_err()
        );
        // 多行现在是合法语义：一行发一条消息，用户自己决定发几条。
        let multiline = validate_reply_items(
            &help_song_entry(),
            vec![item("help.song_request", "第一行\n第二行".to_owned())],
        )
        .expect("多行回复应被接受");
        assert_eq!(multiline.len(), 1, "多行仍是一条配置项");
        // 回车符仍然拒绝：它只会在文案里留下不可见控制字符。
        assert!(
            validate_reply_items(
                &help_song_entry(),
                vec![item("help.song_request", "第一行\r第二行".to_owned())],
            )
            .is_err()
        );
        assert!(
            validate_reply_items(
                &help_song_entry(),
                vec![item("help.song_request", "未知{HTTP内部字段}".to_owned())],
            )
            .is_err()
        );
    }

    #[test]
    fn empty_reply_list_is_valid_for_explicit_mute() {
        assert!(
            validate_reply_items(&help_song_entry(), Vec::new())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 排查：哪些写法会被判成「未知变量占位符」。
    #[test]
    fn reply_variables_valid_edge_cases() {
        // 覆盖常见写法，确认哪些被拒。
        for (text, expected) in [
            ("{玩家}", true),
            ("{{玩家}}", false),
            ("{UID}", true),
            ("{昵称}", false),
            ("{歌名}", true),
            ("{点歌人}", true),
            ("{玩家}点了{歌名}", true),
            ("{{玩家}}点了{歌名}", false),
        ] {
            let actual = reply_variables_valid(text);
            assert_eq!(actual, expected, "输入 {text:?}");
        }

        // 报错必须指明是哪个变量、该怎么写，否则用户只能猜。
        let double = validate_reply_variables("{{玩家}}点了歌").unwrap_err();
        assert!(double.contains("单花括号"), "实际: {double}");
        assert!(double.contains("玩家"), "实际: {double}");

        let unknown = validate_reply_variables("你好{昵称}").unwrap_err();
        assert!(unknown.contains("昵称"), "实际: {unknown}");

        let spaced = validate_reply_variables("{ 玩家 }").unwrap_err();
        assert!(spaced.contains("空格"), "实际: {spaced}");

        // 未闭合或空花括号按普通文本处理，渲染器同样如此。
        assert!(validate_reply_variables("只有左括号{").is_ok());
        assert!(validate_reply_variables("空{}").is_ok());

        // 全局标签与 AI 标签必须放行：面板把它们列在变量按钮里，
        // 校验若只认常规变量，就会出现「列出来却存不进去」。
        for name in crate::features::global_tags::GLOBAL_TAGS
            .iter()
            .map(|t| t.name)
        {
            assert!(
                validate_reply_variables(&format!("好{{}}{name}}}")).is_ok(),
                "全局标签 {{{name}}} 应被接受"
            );
        }
        assert!(
            validate_reply_variables(&format!("好{{{}}}", crate::features::global_tags::AI_TAG))
                .is_ok(),
            "AI 标签应被接受"
        );
    }
}
