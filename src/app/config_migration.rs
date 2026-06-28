use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use serde_yaml::{Mapping, Value};

pub const CURRENT_CONFIG_VERSION: u32 = 4;

struct ChangedDefaultField {
    path: &'static str,
    old_default: u64,
    changed_in_version: u32,
}

const CHANGED_DEFAULT_FIELDS: &[ChangedDefaultField] = &[ChangedDefaultField {
    path: "queue.auto_advance_seconds",
    old_default: 5,
    changed_in_version: 4,
}];

const MOVED_FIELDS: &[(&str, &str)] = &[
    (
        "window.active_check_timeout_ms",
        "timing.active_check_timeout_ms",
    ),
    ("ocr.poll_interval_ms", "timing.chat_scan_fallback_ms"),
    ("ocr.poll_interval_ms", "timing.invite_confirm_poll_ms"),
    ("ocr.change_poll_interval_ms", "timing.scan_loop_idle_ms"),
    ("ocr.change_debounce_ms", "timing.chat_change_debounce_ms"),
    ("ocr.change_cooldown_ms", "timing.chat_change_cooldown_ms"),
    (
        "ocr.post_command_settle_ms",
        "timing.post_command_settle_ms",
    ),
    ("output.paste_timeout_ms", "timing.command_ui_timeout_ms"),
    ("output.focus_delay_ms", "timing.output_focus_ms"),
    ("output.open_chat_delay_ms", "timing.output_open_chat_ms"),
    ("output.click_delay_ms", "timing.output_click_ms"),
    ("output.input_delay_ms", "timing.output_input_ms"),
    ("output.send_delay_ms", "timing.output_send_ms"),
    ("feeluown.timeout_ms", "timing.feeluown_rpc_timeout_ms"),
    ("invite.step_delay_ms", "timing.invite_step_ms"),
    (
        "invite.confirm_timeout_ms",
        "timing.invite_confirm_timeout_ms",
    ),
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

    const DEFAULT: &str = r#"# test config
# version comment
config_version: 4

timing:
  # fallback comment
  chat_scan_fallback_ms: 2000
  scan_loop_idle_ms: 60
  output_focus_ms: 300

queue:
  auto_advance_seconds: 2

ocr:
  min_confidence: 0.9
  change_mean_threshold: 6.0

output:
  send_enabled: true
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

        assert!(report.text.contains("# fallback comment"));
        assert!(report.text.contains("config_version: 4"));
        assert!(report.text.contains("chat_scan_fallback_ms: 1234"));
        assert!(report.text.contains("scan_loop_idle_ms: 77"));
        assert!(report.text.contains("output_focus_ms: 456"));
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
    fn current_version_without_moved_fields_does_not_migrate() {
        let current = r#"config_version: 4
timing:
  chat_scan_fallback_ms: 2000
  scan_loop_idle_ms: 60
  output_focus_ms: 300
queue:
  auto_advance_seconds: 2
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
    fn future_version_does_not_migrate() {
        let future = r#"config_version: 999
timing:
  chat_scan_fallback_ms: 2000
"#;

        let report = migrate_config_text(future, DEFAULT).expect("migration check succeeds");
        assert!(report.is_none());
    }

    #[test]
    fn keeps_existing_new_field_over_moved_old_field() {
        let old = r#"timing:
  chat_scan_fallback_ms: 3333
ocr:
  poll_interval_ms: 1234
"#;

        let report = migrate_config_text(old, DEFAULT)
            .expect("migration succeeds")
            .expect("migration needed");

        assert!(report.text.contains("chat_scan_fallback_ms: 3333"));
        assert!(!report.text.contains("chat_scan_fallback_ms: 1234"));
    }

    #[test]
    fn migrates_old_auto_advance_default_to_two_seconds() {
        let old = r#"config_version: 3
queue:
  auto_advance_seconds: 5
"#;

        let report = migrate_config_text(old, DEFAULT)
            .expect("migration succeeds")
            .expect("migration needed");

        assert!(report.text.contains("auto_advance_seconds: 2"));
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
