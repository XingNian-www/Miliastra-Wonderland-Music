use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use serde_yaml::{Mapping, Value};

pub const CURRENT_CONFIG_VERSION: u32 = 18;

struct ChangedDefaultField {
    path: &'static str,
    old_default: u64,
    changed_in_version: u32,
}

const CHANGED_DEFAULT_FIELDS: &[ChangedDefaultField] = &[
    ChangedDefaultField {
        path: "queue.auto_advance_seconds",
        old_default: 5,
        changed_in_version: 4,
    },
    ChangedDefaultField {
        path: "queue.auto_advance_seconds",
        old_default: 2,
        changed_in_version: 5,
    },
];

struct ChangedBoolDefaultField {
    path: &'static str,
    old_default: bool,
    changed_in_version: u32,
}

const CHANGED_BOOL_DEFAULT_FIELDS: &[ChangedBoolDefaultField] = &[ChangedBoolDefaultField {
    path: "tui.enabled",
    old_default: false,
    changed_in_version: 6,
}];

struct ChangedFloatDefaultField {
    path: &'static str,
    old_default: f64,
    changed_in_version: u32,
}

const CHANGED_FLOAT_DEFAULT_FIELDS: &[ChangedFloatDefaultField] = &[ChangedFloatDefaultField {
    path: "custom_workflows.default_threshold",
    old_default: 0.82,
    changed_in_version: 10,
}];

const MOVED_FIELDS: &[(&str, &str)] = &[
    ("ocr.poll_interval_ms", "timing.chat_scan.fallback_ms"),
    ("ocr.poll_interval_ms", "timing.invite.confirm_poll_ms"),
    ("ocr.change_poll_interval_ms", "timing.loop_idle_ms"),
    (
        "ocr.change_debounce_ms",
        "timing.chat_scan.change_debounce_ms",
    ),
    (
        "ocr.change_cooldown_ms",
        "timing.chat_scan.change_cooldown_ms",
    ),
    (
        "ocr.post_command_settle_ms",
        "timing.command.post_settle_ms",
    ),
    ("output.paste_timeout_ms", "timing.command.ui_timeout_ms"),
    ("output.focus_delay_ms", "timing.input.focus_ms"),
    ("output.open_chat_delay_ms", "timing.input.open_chat_ms"),
    ("output.click_delay_ms", "timing.input.click_ms"),
    ("output.input_delay_ms", "timing.input.text_ms"),
    ("output.send_delay_ms", "timing.input.send_ms"),
    (
        "feeluown.timeout_ms",
        "timing.external.feeluown_rpc_timeout_ms",
    ),
    ("invite.step_delay_ms", "timing.invite.step_ms"),
    (
        "invite.confirm_timeout_ms",
        "timing.invite.confirm_timeout_ms",
    ),
    ("timing.scan_loop_idle_ms", "timing.loop_idle_ms"),
    (
        "timing.chat_scan_fallback_ms",
        "timing.chat_scan.fallback_ms",
    ),
    (
        "timing.chat_change_debounce_ms",
        "timing.chat_scan.change_debounce_ms",
    ),
    (
        "timing.chat_change_cooldown_ms",
        "timing.chat_scan.change_cooldown_ms",
    ),
    (
        "timing.command_ui_timeout_ms",
        "timing.command.ui_timeout_ms",
    ),
    (
        "timing.return_to_primary_retry_ms",
        "timing.command.return_retry_ms",
    ),
    (
        "timing.post_command_settle_ms",
        "timing.command.post_settle_ms",
    ),
    ("timing.help_batch_ms", "timing.command.help_batch_ms"),
    (
        "timing.active_after_activate_ms",
        "timing.input.after_activate_ms",
    ),
    ("timing.output_focus_ms", "timing.input.focus_ms"),
    ("timing.output_open_chat_ms", "timing.input.open_chat_ms"),
    ("timing.output_click_ms", "timing.input.click_ms"),
    ("timing.output_input_ms", "timing.input.text_ms"),
    ("timing.output_send_ms", "timing.input.send_ms"),
    ("timing.hall_page_settle_ms", "timing.hall.page_settle_ms"),
    (
        "timing.hall_ocr_sample_interval_ms",
        "timing.hall.ocr_sample_interval_ms",
    ),
    ("timing.invite_open_chat_ms", "timing.invite.open_chat_ms"),
    ("timing.invite_step_ms", "timing.invite.step_ms"),
    (
        "timing.invite_confirm_timeout_ms",
        "timing.invite.confirm_timeout_ms",
    ),
    (
        "timing.invite_confirm_poll_ms",
        "timing.invite.confirm_poll_ms",
    ),
    (
        "timing.play_search_settle_ms",
        "timing.playback.search_settle_ms",
    ),
    (
        "timing.play_status_poll_ms",
        "timing.playback.status_poll_ms",
    ),
    (
        "timing.play_status_retries",
        "timing.playback.status_retries",
    ),
    (
        "timing.skip_status_initial_ms",
        "timing.playback.skip_status_initial_ms",
    ),
    (
        "timing.skip_status_poll_ms",
        "timing.playback.skip_status_poll_ms",
    ),
    (
        "timing.skip_status_retries",
        "timing.playback.skip_status_retries",
    ),
    (
        "timing.playback_monitor_tick_ms",
        "timing.playback.monitor_tick_ms",
    ),
    (
        "timing.playback_monitor_status_ms",
        "timing.playback.monitor_status_ms",
    ),
    ("timing.decision_timeout_ms", "timing.decision.timeout_ms"),
    ("timing.decision_poll_ms", "timing.decision.poll_ms"),
    (
        "timing.feeluown_rpc_timeout_ms",
        "timing.external.feeluown_rpc_timeout_ms",
    ),
    (
        "timing.volume_smooth_step_ms",
        "timing.external.volume_smooth_step_ms",
    ),
    (
        "timing.ai_request_timeout_ms",
        "timing.external.ai_request_timeout_ms",
    ),
    (
        "custom_workflows.default_timeout_ms",
        "timing.workflow.default_timeout_ms",
    ),
    (
        "custom_workflows.default_poll_ms",
        "timing.workflow.default_poll_ms",
    ),
    (
        "custom_workflows.default_step_wait_ms",
        "timing.workflow.default_step_wait_ms",
    ),
    (
        "moderation.vote_timeout_ms",
        "timing.moderation.vote_timeout_ms",
    ),
    ("moderation.vote_poll_ms", "timing.moderation.vote_poll_ms"),
    (
        "moderation.search_result_timeout_ms",
        "timing.moderation.search_result_timeout_ms",
    ),
    (
        "moderation.confirm_wait_ms",
        "timing.moderation.confirm_wait_ms",
    ),
    ("startup.game_path", "startup.exe_path"),
    ("templates.dating", "templates.secondary_hall"),
];

#[derive(Debug)]
pub struct MigrationReport {
    pub text: String,
    pub old_version: Option<u64>,
    pub migrated_count: usize,
    pub unmigrated: Vec<UnmigratedItem>,
}

#[derive(Clone, Debug)]
pub struct UnmigratedItem {
    pub path: String,
    pub value: Value,
    pub reason: String,
}

pub fn migrate_config_text(old_text: &str, default_text: &str) -> Result<Option<MigrationReport>> {
    let old_value: Value = serde_yaml::from_str(old_text).context("parse existing config yaml")?;
    let default_value: Value =
        serde_yaml::from_str(default_text).context("parse default config yaml")?;
    let old_version = get_path(&old_value, &["config_version"]).and_then(Value::as_u64);
    if old_version.is_some_and(|version| version > CURRENT_CONFIG_VERSION as u64) {
        return Ok(None);
    }
    if old_version == Some(CURRENT_CONFIG_VERSION as u64) && !has_moved_source(&old_value) {
        return Ok(None);
    }

    let mut new_value = default_value.clone();
    let mut used_paths = BTreeSet::new();
    let mut unmigrated = Vec::new();
    let mut migrated_count = 0;

    if get_path(&old_value, &["config_version"]).is_some() {
        used_paths.insert("config_version".to_string());
    }

    copy_common_fields(
        &old_value,
        &default_value,
        &mut new_value,
        &mut Vec::new(),
        &mut used_paths,
        &mut unmigrated,
        &mut migrated_count,
    );
    migrate_moved_fields(
        &old_value,
        &default_value,
        &mut new_value,
        &mut used_paths,
        &mut unmigrated,
        &mut migrated_count,
    );
    migrate_changed_default_fields(&old_value, &default_value, &mut new_value, old_version);
    normalize_custom_workflows(&mut new_value);
    set_path(
        &mut new_value,
        &["config_version"],
        Value::Number((CURRENT_CONFIG_VERSION as u64).into()),
    )?;

    collect_unmigrated_fields(
        &old_value,
        &default_value,
        &mut Vec::new(),
        &used_paths,
        &mut unmigrated,
    );

    let mut text = default_text.to_string();
    let mut paths = Vec::new();
    collect_template_paths(&default_value, &mut Vec::new(), &mut paths);
    for path in paths {
        let default_leaf = get_path(&default_value, &path_as_strs(&path))
            .ok_or_else(|| anyhow!("default config value missing: {}", path.join(".")))?;
        let value = get_path(&new_value, &path_as_strs(&path))
            .ok_or_else(|| anyhow!("migrated config value missing: {}", path.join(".")))?;
        if value == default_leaf {
            continue;
        }
        replace_template_value(&mut text, &path, value)
            .with_context(|| format!("render migrated config path {}", path.join(".")))?;
    }
    append_unmigrated_block(&mut text, &unmigrated)?;

    Ok(Some(MigrationReport {
        text,
        old_version,
        migrated_count,
        unmigrated,
    }))
}

fn migrate_changed_default_fields(
    old_value: &Value,
    default_value: &Value,
    new_value: &mut Value,
    old_version: Option<u64>,
) {
    for field in CHANGED_DEFAULT_FIELDS {
        if old_version.is_some_and(|version| version >= field.changed_in_version as u64) {
            continue;
        }
        let path = split_path(field.path);
        if get_path(old_value, &path).and_then(Value::as_u64) != Some(field.old_default) {
            continue;
        }
        let Some(current_default) = get_path(default_value, &path) else {
            continue;
        };
        let _ = set_path(new_value, &path, current_default.clone());
    }
    for field in CHANGED_BOOL_DEFAULT_FIELDS {
        if old_version.is_some_and(|version| version >= field.changed_in_version as u64) {
            continue;
        }
        let path = split_path(field.path);
        if get_path(old_value, &path).and_then(Value::as_bool) != Some(field.old_default) {
            continue;
        }
        let Some(current_default) = get_path(default_value, &path) else {
            continue;
        };
        let _ = set_path(new_value, &path, current_default.clone());
    }
    for field in CHANGED_FLOAT_DEFAULT_FIELDS {
        if old_version.is_some_and(|version| version >= field.changed_in_version as u64) {
            continue;
        }
        let path = split_path(field.path);
        let Some(old_value) = get_path(old_value, &path).and_then(Value::as_f64) else {
            continue;
        };
        if (old_value - field.old_default).abs() > f64::EPSILON {
            continue;
        }
        let Some(current_default) = get_path(default_value, &path) else {
            continue;
        };
        let _ = set_path(new_value, &path, current_default.clone());
    }
}

fn normalize_custom_workflows(value: &mut Value) {
    let Some(Value::Sequence(workflows)) = get_path_mut(value, &["custom_workflows", "workflows"])
    else {
        return;
    };
    for workflow in workflows {
        let Value::Mapping(workflow_map) = workflow else {
            continue;
        };
        insert_missing(workflow_map, "enabled", Value::Bool(true));
        insert_missing(workflow_map, "name", Value::String(String::new()));
        insert_missing(workflow_map, "commands", Value::Sequence(Vec::new()));
        insert_missing(workflow_map, "allow_args", Value::Bool(false));
        insert_missing(
            workflow_map,
            "message_types",
            Value::Sequence(vec![Value::String("blue".to_string())]),
        );
        insert_missing(workflow_map, "confirm_before_run", Value::Bool(false));
        insert_missing(
            workflow_map,
            "confirm_message",
            Value::String(String::new()),
        );
        insert_missing(
            workflow_map,
            "confirm_message_types",
            Value::Sequence(vec![Value::String("blue".to_string())]),
        );
        insert_missing(workflow_map, "steps", Value::Sequence(Vec::new()));
        insert_missing(
            workflow_map,
            "success_message",
            Value::String(String::new()),
        );

        let Some(Value::Sequence(steps)) = map_get_mut(workflow_map, "steps") else {
            continue;
        };
        for step in steps {
            let Value::Mapping(step_map) = step else {
                continue;
            };
            insert_missing(step_map, "type", Value::String(String::new()));
        }
    }
}

fn insert_missing(map: &mut Mapping, key: &str, value: Value) {
    map.entry(Value::String(key.to_string())).or_insert(value);
}

pub fn backup_path(path: &Path) -> PathBuf {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.yaml");
    path.with_file_name(format!("{file_name}.bak-{seconds}"))
}

fn copy_common_fields(
    old_value: &Value,
    default_value: &Value,
    new_value: &mut Value,
    path: &mut Vec<String>,
    used_paths: &mut BTreeSet<String>,
    unmigrated: &mut Vec<UnmigratedItem>,
    migrated_count: &mut usize,
) {
    match (old_value, default_value) {
        (Value::Mapping(old_map), Value::Mapping(default_map)) => {
            if default_map.is_empty() && !path.is_empty() {
                if set_path(new_value, &path_as_strs(path), old_value.clone()).is_ok() {
                    used_paths.insert(path_key(path));
                    if old_value != default_value {
                        *migrated_count += 1;
                    }
                }
                return;
            }
            for (key, old_child) in old_map {
                let Some(key) = key.as_str() else {
                    continue;
                };
                let key_value = Value::String(key.to_string());
                let Some(default_child) = default_map.get(&key_value) else {
                    continue;
                };
                path.push(key.to_string());
                copy_common_fields(
                    old_child,
                    default_child,
                    new_value,
                    path,
                    used_paths,
                    unmigrated,
                    migrated_count,
                );
                path.pop();
            }
        }
        _ => {
            if path.as_slice() == ["config_version"] {
                used_paths.insert(path_key(path));
                return;
            }
            if same_shape(old_value, default_value) {
                if set_path(new_value, &path_as_strs(path), old_value.clone()).is_ok() {
                    used_paths.insert(path_key(path));
                    *migrated_count += 1;
                }
            } else {
                used_paths.insert(path_key(path));
                unmigrated.push(UnmigratedItem {
                    path: path_key(path),
                    value: old_value.clone(),
                    reason: "类型和当前配置不一致，已保留默认值".to_string(),
                });
            }
        }
    }
}

fn migrate_moved_fields(
    old_value: &Value,
    default_value: &Value,
    new_value: &mut Value,
    used_paths: &mut BTreeSet<String>,
    unmigrated: &mut Vec<UnmigratedItem>,
    migrated_count: &mut usize,
) {
    for (source, target) in MOVED_FIELDS {
        let source_path = split_path(source);
        let target_path = split_path(target);
        let Some(source_value) = get_path(old_value, &source_path) else {
            continue;
        };
        used_paths.insert((*source).to_string());
        let Some(default_target) = get_path(default_value, &target_path) else {
            unmigrated.push(UnmigratedItem {
                path: (*source).to_string(),
                value: source_value.clone(),
                reason: format!("当前版本缺少目标配置项 {target}"),
            });
            continue;
        };
        if get_path(old_value, &target_path).is_some_and(|value| same_shape(value, default_target))
        {
            continue;
        }
        if !same_shape(source_value, default_target) {
            unmigrated.push(UnmigratedItem {
                path: (*source).to_string(),
                value: source_value.clone(),
                reason: format!("无法迁移到 {target}，类型和当前配置不一致"),
            });
            continue;
        }
        if set_path(new_value, &target_path, source_value.clone()).is_ok() {
            *migrated_count += 1;
        }
    }
}

fn collect_unmigrated_fields(
    old_value: &Value,
    default_value: &Value,
    path: &mut Vec<String>,
    used_paths: &BTreeSet<String>,
    unmigrated: &mut Vec<UnmigratedItem>,
) {
    let key = path_key(path);
    if !key.is_empty() && used_paths.contains(&key) {
        return;
    }
    if !path.is_empty() && get_path(default_value, &path_as_strs(path)).is_none() {
        unmigrated.push(UnmigratedItem {
            path: key,
            value: old_value.clone(),
            reason: "当前版本没有对应配置项".to_string(),
        });
        return;
    }
    let Value::Mapping(map) = old_value else {
        return;
    };
    for (child_key, child_value) in map {
        let Some(child_key) = child_key.as_str() else {
            continue;
        };
        path.push(child_key.to_string());
        collect_unmigrated_fields(child_value, default_value, path, used_paths, unmigrated);
        path.pop();
    }
}

fn collect_template_paths(value: &Value, path: &mut Vec<String>, output: &mut Vec<Vec<String>>) {
    match value {
        Value::Mapping(map) if map.is_empty() => output.push(path.clone()),
        Value::Mapping(map) => {
            for (key, child) in map {
                let Some(key) = key.as_str() else {
                    continue;
                };
                path.push(key.to_string());
                collect_template_paths(child, path, output);
                path.pop();
            }
        }
        _ => output.push(path.clone()),
    }
}

fn replace_template_value(text: &mut String, path: &[String], value: &Value) -> Result<()> {
    let mut lines = text.lines().map(str::to_string).collect::<Vec<_>>();
    let mut stack: Vec<(usize, String)> = Vec::new();
    for index in 0..lines.len() {
        let Some((indent, key)) = parse_key_line(&lines[index]) else {
            continue;
        };
        while stack.last().is_some_and(|(level, _)| *level >= indent) {
            stack.pop();
        }
        let mut current = stack.iter().map(|(_, key)| key.clone()).collect::<Vec<_>>();
        current.push(key.clone());
        if current == path {
            let replacement = render_key_value(indent, &key, value)?;
            let end = if is_inline_scalar(value) {
                index + 1
            } else {
                block_end(&lines, index, indent)
            };
            lines.splice(index..end, replacement);
            *text = lines.join("\n");
            text.push('\n');
            return Ok(());
        }
        stack.push((indent, key));
    }
    Err(anyhow!("template path not found: {}", path.join(".")))
}

fn render_key_value(indent: usize, key: &str, value: &Value) -> Result<Vec<String>> {
    let prefix = format!("{}{}:", " ".repeat(indent), key);
    if is_inline_scalar(value) {
        return Ok(vec![format!("{} {}", prefix, scalar_yaml(value)?)]);
    }
    let mut lines = vec![prefix];
    for line in yaml_lines(value)? {
        lines.push(format!("{}{}", " ".repeat(indent + 2), line));
    }
    Ok(lines)
}

fn append_unmigrated_block(text: &mut String, items: &[UnmigratedItem]) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push('\n');
    text.push_str("# 未自动迁移的旧配置（已注释，不影响运行）\n");
    text.push_str("# 请手动确认这些字段是否还需要保留或改名。\n");
    for item in items {
        text.push_str("#\n");
        text.push_str(&format!("# {}：{}\n", item.path, item.reason));
        if is_inline_scalar(&item.value) {
            text.push_str(&format!("# {}: {}\n", item.path, scalar_yaml(&item.value)?));
        } else {
            text.push_str(&format!("# {}:\n", item.path));
            for line in yaml_lines(&item.value)? {
                text.push_str("#   ");
                text.push_str(&line);
                text.push('\n');
            }
        }
    }
    Ok(())
}

fn parse_key_line(line: &str) -> Option<(usize, String)> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('-') {
        return None;
    }
    let indent = line.len() - trimmed.len();
    let (key, _) = trimmed.split_once(':')?;
    Some((indent, key.trim().trim_matches('"').to_string()))
}

fn block_end(lines: &[String], start: usize, indent: usize) -> usize {
    let mut end = start + 1;
    while end < lines.len() {
        let line = &lines[end];
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            break;
        }
        let line_indent = line.len() - trimmed.len();
        if trimmed.starts_with('#') && line_indent <= indent {
            break;
        }
        if !trimmed.starts_with('#') && line_indent <= indent {
            break;
        }
        end += 1;
    }
    end
}

fn has_moved_source(value: &Value) -> bool {
    MOVED_FIELDS
        .iter()
        .any(|(source, _)| get_path(value, &split_path(source)).is_some())
}

fn same_shape(value: &Value, template: &Value) -> bool {
    matches!(
        (value, template),
        (Value::Bool(_), Value::Bool(_))
            | (Value::Number(_), Value::Number(_))
            | (Value::String(_), Value::String(_))
            | (Value::Sequence(_), Value::Sequence(_))
            | (Value::Mapping(_), Value::Mapping(_))
    )
}

fn get_path<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = value;
    for key in path {
        let Value::Mapping(map) = current else {
            return None;
        };
        current = map_get(map, key)?;
    }
    Some(current)
}

fn get_path_mut<'a>(value: &'a mut Value, path: &[&str]) -> Option<&'a mut Value> {
    let mut current = value;
    for key in path {
        let Value::Mapping(map) = current else {
            return None;
        };
        current = map_get_mut(map, key)?;
    }
    Some(current)
}

fn set_path(value: &mut Value, path: &[&str], replacement: Value) -> Result<()> {
    let Some((last, parents)) = path.split_last() else {
        return Err(anyhow!("empty config path"));
    };
    let mut current = value;
    for key in parents {
        let Value::Mapping(map) = current else {
            return Err(anyhow!("config path is not a map: {}", path.join(".")));
        };
        current = map_get_mut(map, key)
            .ok_or_else(|| anyhow!("config path not found: {}", path.join(".")))?;
    }
    let Value::Mapping(map) = current else {
        return Err(anyhow!("config path is not a map: {}", path.join(".")));
    };
    map.insert(Value::String((*last).to_string()), replacement);
    Ok(())
}

fn map_get<'a>(map: &'a Mapping, key: &str) -> Option<&'a Value> {
    map.get(Value::String(key.to_string()))
}

fn map_get_mut<'a>(map: &'a mut Mapping, key: &str) -> Option<&'a mut Value> {
    map.get_mut(Value::String(key.to_string()))
}

fn path_as_strs(path: &[String]) -> Vec<&str> {
    path.iter().map(String::as_str).collect()
}

fn split_path(path: &str) -> Vec<&str> {
    path.split('.').collect()
}

fn path_key(path: &[String]) -> String {
    path.join(".")
}

fn is_inline_scalar(value: &Value) -> bool {
    matches!(
        value,
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
    )
}

fn scalar_yaml(value: &Value) -> Result<String> {
    Ok(serde_yaml::to_string(value)
        .context("serialize yaml scalar")?
        .trim()
        .to_string())
}

fn yaml_lines(value: &Value) -> Result<Vec<String>> {
    Ok(serde_yaml::to_string(value)
        .context("serialize yaml value")?
        .lines()
        .filter(|line| !line.trim().is_empty() && *line != "---")
        .map(str::to_string)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT: &str = r#"# 测试配置
# 版本注释
config_version: 18

timing:
  watchdog_restart_ms: 2000
  loop_idle_ms: 60
  chat_scan:
    # 兜底扫描注释
    fallback_ms: 2000
    change_debounce_ms: 120
    change_cooldown_ms: 250
  command:
    ui_timeout_ms: 15000
    return_retry_ms: 1000
    post_settle_ms: 500
    help_batch_ms: 500
  input:
    after_activate_ms: 200
    focus_ms: 300
    open_chat_ms: 300
    click_ms: 150
    text_ms: 250
    send_ms: 300
  workflow:
    default_timeout_ms: 5000
    default_poll_ms: 200
    default_step_wait_ms: 300
  hall:
    page_settle_ms: 800
    ocr_sample_interval_ms: 120
  invite:
    open_chat_ms: 400
    step_ms: 800
    confirm_timeout_ms: 30000
    confirm_poll_ms: 2000
  moderation:
    vote_timeout_ms: 120000
    vote_poll_ms: 2000
    search_result_timeout_ms: 5000
    confirm_wait_ms: 2000
  playback:
    search_settle_ms: 2000
    status_poll_ms: 1000
    status_retries: 15
    skip_status_initial_ms: 500
    skip_status_poll_ms: 300
    skip_status_retries: 5
    monitor_tick_ms: 200
    monitor_status_ms: 1000
  decision:
    timeout_ms: 20000
    poll_ms: 2000
  external:
    feeluown_rpc_timeout_ms: 10000
    volume_smooth_step_ms: 300
    ai_request_timeout_ms: 35000

queue:
  auto_advance_seconds: 1
  protect_auto_played_songs: true
  protect_current_song_until_finished: true

song_review:
  enabled: false
  max_allowed_level: 4
  failure_policy: reject
  retry_count: 2
  retry_delay_ms: 500
  reply_reason_max_chars: 40
  custom_prompt: |
    拒绝明显低俗、辱骂、擦边、引战、暴力、政治敏感、广告导流或不适合公开大厅播放的歌曲。
    对信息不足但看起来正常的歌曲给较低风险分，不要凭空猜测。
  provider:
    endpoint: ""
    api_key: ""
    model: ""

tui:
  enabled: true

startup:
  enabled: true
  launch_game: true
  enter_game: true
  enter_wonderland: true
  exe_path: ""
  game_args: ""
  launch_wait_ms: 5000
  launch_retries: 12
  enter_game_timeout_ms: 60000
  enter_wonderland_timeout_ms: 300000
  wonderland_home_retries: 120
  wonderland_home_retry_ms: 2500
  wonderland_card_retries: 90
  wonderland_card_retry_ms: 2000
  wonderland_confirm_absent_timeout_ms: 60000
  wonderland_confirm_stable_timeout_ms: 60000
  final_primary_timeout_ms: 120000
  poll_ms: 1000
  stable_mean_threshold: 2.0
  stable_changed_ratio_threshold: 0.01
  template_threshold: 0.9
  wonderland_enter_button_threshold: 0.9
  templates:
    wonderland_enter_button: assets/startup-confirm-black.png
    paimon_menu: assets/startup-paimon-menu.png
    wonderland_close: assets/startup-wonderland-close.png
  enter_game_text_region:
    x: 900
    y: 1000
    width: 130
    height: 40
  wonderland_enter_button_region:
    x: 1400
    y: 850
    width: 360
    height: 150
  main_ui_region:
    x: 0
    y: 0
    width: 480
    height: 270
  wonderland_close_region:
    x: 1780
    y: 0
    width: 140
    height: 90
  wonderland_card_point:
    x: 680
    y: 310

ocr:
  min_confidence: 0.9
  change_mean_threshold: 6.0

templates:
  secondary_hall: assets/ui-secondary-hall.png

output:
  send_enabled: true

custom_workflows:
  enabled: true
  default_threshold: 0.9
  templates: {}
  workflows: []
"#;

    #[test]
    fn migrates_values_into_default_template() {
        let old = r#"ocr:
  min_confidence: 0.8
  poll_interval_ms: 1234
  change_poll_interval_ms: 77
output:
  focus_delay_ms: 456
unknown_root:
  enabled: true
"#;

        let report = migrate_config_text(old, DEFAULT)
            .expect("migration succeeds")
            .expect("migration needed");

        assert!(report.text.contains("# 兜底扫描注释"));
        assert!(report.text.contains("config_version: 18"));
        assert!(report.text.contains("template_threshold: 0.9"));
        assert!(report.text.contains("enter_game_timeout_ms: 60000"));
        assert!(report.text.contains("enter_game_text_region:"));
        assert!(report.text.contains("x: 900"));
        assert!(report.text.contains("wonderland_close_region:"));
        assert!(report.text.contains("fallback_ms: 1234"));
        assert!(report.text.contains("loop_idle_ms: 77"));
        assert!(report.text.contains("focus_ms: 456"));
        assert!(report.text.contains("min_confidence: 0.8"));
        assert!(report.text.contains("# 未自动迁移的旧配置"));
        assert!(
            report
                .text
                .contains("# unknown_root：当前版本没有对应配置项")
        );
        assert!(
            report
                .unmigrated
                .iter()
                .any(|item| item.path == "unknown_root")
        );
    }

    #[test]
    fn migrates_custom_workflow_timing_defaults_to_timing_workflow() {
        let old = r#"config_version: 12
custom_workflows:
  default_timeout_ms: 9000
  default_poll_ms: 333
  default_step_wait_ms: 444
"#;

        let report = migrate_config_text(old, DEFAULT)
            .expect("migration succeeds")
            .expect("migration needed");
        let migrated: Value = serde_yaml::from_str(&report.text).expect("valid migrated yaml");

        assert_eq!(
            get_path(&migrated, &["timing", "workflow", "default_timeout_ms"])
                .and_then(Value::as_u64),
            Some(9000)
        );
        assert_eq!(
            get_path(&migrated, &["timing", "workflow", "default_poll_ms"]).and_then(Value::as_u64),
            Some(333)
        );
        assert_eq!(
            get_path(&migrated, &["timing", "workflow", "default_step_wait_ms"])
                .and_then(Value::as_u64),
            Some(444)
        );
        assert!(
            !report
                .unmigrated
                .iter()
                .any(|item| item.path.starts_with("custom_workflows.default_"))
        );
    }

    #[test]
    fn migrates_flat_timing_fields_to_grouped_timing() {
        let old = r#"config_version: 12
timing:
  scan_loop_idle_ms: 70
  chat_scan_fallback_ms: 2100
  chat_change_debounce_ms: 140
  command_ui_timeout_ms: 16000
  output_focus_ms: 310
  invite_confirm_poll_ms: 1900
  play_status_retries: 17
  decision_timeout_ms: 22000
  feeluown_rpc_timeout_ms: 12000
"#;

        let report = migrate_config_text(old, DEFAULT)
            .expect("migration succeeds")
            .expect("migration needed");
        let migrated: Value = serde_yaml::from_str(&report.text).expect("valid migrated yaml");

        assert_eq!(
            get_path(&migrated, &["timing", "loop_idle_ms"]).and_then(Value::as_u64),
            Some(70)
        );
        assert_eq!(
            get_path(&migrated, &["timing", "chat_scan", "fallback_ms"]).and_then(Value::as_u64),
            Some(2100)
        );
        assert_eq!(
            get_path(&migrated, &["timing", "chat_scan", "change_debounce_ms"])
                .and_then(Value::as_u64),
            Some(140)
        );
        assert_eq!(
            get_path(&migrated, &["timing", "command", "ui_timeout_ms"]).and_then(Value::as_u64),
            Some(16000)
        );
        assert_eq!(
            get_path(&migrated, &["timing", "input", "focus_ms"]).and_then(Value::as_u64),
            Some(310)
        );
        assert_eq!(
            get_path(&migrated, &["timing", "invite", "confirm_poll_ms"]).and_then(Value::as_u64),
            Some(1900)
        );
        assert_eq!(
            get_path(&migrated, &["timing", "playback", "status_retries"]).and_then(Value::as_u64),
            Some(17)
        );
        assert_eq!(
            get_path(&migrated, &["timing", "decision", "timeout_ms"]).and_then(Value::as_u64),
            Some(22000)
        );
        assert_eq!(
            get_path(
                &migrated,
                &["timing", "external", "feeluown_rpc_timeout_ms"]
            )
            .and_then(Value::as_u64),
            Some(12000)
        );
        assert!(
            !report
                .unmigrated
                .iter()
                .any(|item| item.path.starts_with("timing."))
        );
    }

    #[test]
    fn migrates_moderation_timing_defaults_to_timing_moderation() {
        let old = r#"config_version: 12
moderation:
  vote_timeout_ms: 90000
  vote_poll_ms: 1500
  search_result_timeout_ms: 6000
  confirm_wait_ms: 2500
"#;

        let report = migrate_config_text(old, DEFAULT)
            .expect("migration succeeds")
            .expect("migration needed");
        let migrated: Value = serde_yaml::from_str(&report.text).expect("valid migrated yaml");

        assert_eq!(
            get_path(&migrated, &["timing", "moderation", "vote_timeout_ms"])
                .and_then(Value::as_u64),
            Some(90000)
        );
        assert_eq!(
            get_path(&migrated, &["timing", "moderation", "vote_poll_ms"]).and_then(Value::as_u64),
            Some(1500)
        );
        assert_eq!(
            get_path(
                &migrated,
                &["timing", "moderation", "search_result_timeout_ms"]
            )
            .and_then(Value::as_u64),
            Some(6000)
        );
        assert_eq!(
            get_path(&migrated, &["timing", "moderation", "confirm_wait_ms"])
                .and_then(Value::as_u64),
            Some(2500)
        );
        assert!(
            !report
                .unmigrated
                .iter()
                .any(|item| item.path.starts_with("moderation.") && item.path.ends_with("_ms"))
        );
    }

    #[test]
    fn migrates_dating_template_to_secondary_hall_template() {
        let old = r#"config_version: 15
templates:
  dating: assets/custom-dating.png
"#;

        let report = migrate_config_text(old, DEFAULT)
            .expect("migration succeeds")
            .expect("migration needed");
        let migrated: Value = serde_yaml::from_str(&report.text).expect("valid migrated yaml");

        assert_eq!(
            get_path(&migrated, &["templates", "secondary_hall"]).and_then(Value::as_str),
            Some("assets/custom-dating.png")
        );
        assert!(
            !report
                .unmigrated
                .iter()
                .any(|item| item.path == "templates.dating")
        );
    }

    #[test]
    fn current_version_without_moved_fields_does_not_migrate() {
        let current = r#"config_version: 18
timing:
  loop_idle_ms: 60
  chat_scan:
    fallback_ms: 2000
    change_debounce_ms: 120
    change_cooldown_ms: 250
  input:
    focus_ms: 300
queue:
  auto_advance_seconds: 1
  protect_auto_played_songs: true
  protect_current_song_until_finished: true
tui:
  enabled: true
ocr:
  min_confidence: 0.9
  change_mean_threshold: 6.0
output:
  send_enabled: true
"#;

        let report = migrate_config_text(current, DEFAULT).expect("migration check succeeds");
        assert!(report.is_none());
    }

    #[test]
    fn migrates_v10_current_song_protection_to_default_enabled() {
        let old = r#"config_version: 10
queue:
  auto_advance_seconds: 1
  protect_auto_played_songs: true
"#;

        let report = migrate_config_text(old, DEFAULT)
            .expect("migration succeeds")
            .expect("migration needed");

        assert!(report.text.contains("config_version: 18"));
        assert!(
            report
                .text
                .contains("protect_current_song_until_finished: true")
        );
    }

    #[test]
    fn future_version_does_not_migrate() {
        let future = r#"config_version: 999
timing:
  chat_scan:
    fallback_ms: 2000
"#;

        let report = migrate_config_text(future, DEFAULT).expect("migration check succeeds");
        assert!(report.is_none());
    }

    #[test]
    fn keeps_existing_new_field_over_moved_old_field() {
        let old = r#"timing:
  chat_scan:
    fallback_ms: 3333
ocr:
  poll_interval_ms: 1234
"#;

        let report = migrate_config_text(old, DEFAULT)
            .expect("migration succeeds")
            .expect("migration needed");

        assert!(report.text.contains("fallback_ms: 3333"));
        assert!(!report.text.contains("fallback_ms: 1234"));
    }

    #[test]
    fn normalizes_custom_workflow_fields() {
        let old = r#"config_version: 9
custom_workflows:
  workflows:
    - name: example
      commands: [测试流程]
      steps:
        - type: key
          key: F2
"#;

        let report = migrate_config_text(old, DEFAULT)
            .expect("migration succeeds")
            .expect("migration needed");

        assert!(report.text.contains("config_version: 18"));
        assert!(report.text.contains("allow_args: false"));
        assert!(report.text.contains("message_types:"));
        assert!(report.text.contains("confirm_before_run: false"));
        assert!(report.text.contains("confirm_message: ''"));
        assert!(report.text.contains("confirm_message_types:"));
        assert!(report.text.contains("success_message: ''"));
    }

    #[test]
    fn keeps_custom_workflow_template_mappings() {
        let old = r#"config_version: 9
custom_workflows:
  templates:
    my_button: assets/my-button.png
  workflows:
    - name: example
      commands: [测试流程]
      steps:
        - type: click_template
          template: my_button
"#;

        let report = migrate_config_text(old, DEFAULT)
            .expect("migration succeeds")
            .expect("migration needed");

        assert!(report.text.contains("templates:"));
        assert!(report.text.contains("my_button: assets/my-button.png"));
        assert!(
            !report
                .unmigrated
                .iter()
                .any(|item| item.path.starts_with("custom_workflows.templates"))
        );
    }

    #[test]
    fn migrates_custom_workflow_threshold_default_to_current() {
        let old = r#"config_version: 9
custom_workflows:
  default_threshold: 0.82
"#;

        let report = migrate_config_text(old, DEFAULT)
            .expect("migration succeeds")
            .expect("migration needed");

        assert!(report.text.contains("default_threshold: 0.9"));
    }

    #[test]
    fn migrates_v3_auto_advance_default_to_one_second() {
        let old = r#"config_version: 3
queue:
  auto_advance_seconds: 5
"#;

        let report = migrate_config_text(old, DEFAULT)
            .expect("migration succeeds")
            .expect("migration needed");

        assert!(report.text.contains("auto_advance_seconds: 1"));
    }

    #[test]
    fn migrates_v4_auto_advance_default_to_one_second() {
        let old = r#"config_version: 4
queue:
  auto_advance_seconds: 2
"#;

        let report = migrate_config_text(old, DEFAULT)
            .expect("migration succeeds")
            .expect("migration needed");

        assert!(report.text.contains("config_version: 18"));
        assert!(report.text.contains("auto_advance_seconds: 1"));
    }

    #[test]
    fn migrates_v5_tui_default_to_enabled() {
        let old = r#"config_version: 5
tui:
  enabled: false
"#;

        let report = migrate_config_text(old, DEFAULT)
            .expect("migration succeeds")
            .expect("migration needed");

        assert!(report.text.contains("config_version: 18"));
        assert!(report.text.contains("enabled: true"));
        assert!(report.text.contains("protect_auto_played_songs: true"));
    }

    #[test]
    fn keeps_custom_tui_enabled() {
        let old = r#"config_version: 5
tui:
  enabled: true
"#;

        let report = migrate_config_text(old, DEFAULT)
            .expect("migration succeeds")
            .expect("migration needed");

        assert!(report.text.contains("enabled: true"));
    }

    #[test]
    fn keeps_custom_auto_advance_seconds() {
        let old = r#"config_version: 3
queue:
  auto_advance_seconds: 3
"#;

        let report = migrate_config_text(old, DEFAULT)
            .expect("migration succeeds")
            .expect("migration needed");

        assert!(report.text.contains("auto_advance_seconds: 3"));
    }
}
