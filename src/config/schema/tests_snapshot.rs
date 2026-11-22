//! W0-B：schema 的稳定投影快照（只读资产，零行为改动）。
//!
//! 把 [`config_sections`] 的全部声明（38 段 / 399 字段）连同**默认值摘要**、
//! effect 分布与 121 条 Live 清单序列化成 `schema.snapshot.json`。任何 schema
//! 漂移（增删字段、改 effect/kind、改默认值、改段顺序）都会让
//! [`schema_snapshot_is_current`] 失败；确认漂移是**有意**的之后重新生成：
//!
//! ```text
//! UPDATE_SCHEMA_SNAPSHOT=1 cargo test --lib --no-default-features -- config::schema::tests_snapshot
//! ```
//!
//! 快照里不得出现真实凭据：[`SECRET_PATHS`] 与 `FieldKind::Secret` 的默认值一律
//! 写作 `"<secret>"`，并由 [`snapshot_never_leaks_secret_defaults`] 反查兜底。
//!
//! 快照的用途（方案 W0 验收项 4）：让「schema 没漂」成为可 diff、可 review 的
//! 事实，而不是靠读 4,215 行声明去猜。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use super::*;

/// 快照文件相对 crate 根目录的位置。
const SNAPSHOT_RELATIVE_PATH: &str = "src/config/schema/schema.snapshot.json";

/// 默认值摘要中字符串的最大长度；更长的（提示词等）折叠成 `<string len=N>`。
///
/// 折叠而不是截断：长度仍然会随内容变化，所以提示词改字也会让快照失败，
/// 但快照体积不会被 50 段长提示词撑大（也不把提示词正文抄进仓库资产）。
const MAX_INLINE_STRING_CHARS: usize = 80;

/// 默认值摘要中数组的最大内联元素数；更长的折叠成 `<array len=N>`。
///
/// `custom_workflows.workflows` 的默认值是一整张流程表（单条 17KB），
/// 内联进快照会淹没 diff；快照要锁的是 schema，不是默认流程库的内容。
const MAX_INLINE_ARRAY_ITEMS: usize = 3;

/// 默认值摘要中对象的最大内联键数；更长的折叠成 `<object keys=N>`。
const MAX_INLINE_OBJECT_KEYS: usize = 8;

/// 快照的生成脚本路径（写进快照头，方便 review 时定位来源）。
const GENERATED_BY: &str = "src/config/schema/tests_snapshot.rs";

fn snapshot_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(SNAPSHOT_RELATIVE_PATH)
}

/// `FieldKind` → 稳定 JSON 投影（`FieldKind` 本身不是 `Serialize`，也不想为
/// 测试给它加 derive——那是生产类型的改动）。
fn kind_value(kind: &FieldKind) -> Value {
    match kind {
        FieldKind::Bool => json!({"type": "bool"}),
        FieldKind::Int { min, max } => json!({"type": "int", "min": min, "max": max}),
        FieldKind::Float { min, max } => json!({"type": "float", "min": min, "max": max}),
        FieldKind::String => json!({"type": "string"}),
        FieldKind::Text => json!({"type": "text"}),
        FieldKind::Path => json!({"type": "path"}),
        FieldKind::Enum(values) => json!({
            "type": "enum",
            "values": values.iter().map(|(value, _)| value.clone()).collect::<Vec<_>>(),
        }),
        FieldKind::StringArray => json!({"type": "stringArray"}),
        FieldKind::Object => json!({"type": "object"}),
        FieldKind::Rect => json!({"type": "rect"}),
        FieldKind::Point => json!({"type": "point"}),
        FieldKind::Secret => json!({"type": "secret"}),
        FieldKind::ModelName => json!({"type": "modelName"}),
    }
}

/// effect 的短名（与 `Effect` 的 serde 表示一致，但用于计数键）。
fn effect_key(effect: Effect) -> &'static str {
    match effect {
        Effect::Live => "live",
        Effect::IdleReload => "idleReload",
        Effect::PlaybackIdleReload => "playbackIdleReload",
        Effect::Restart => "restart",
    }
}

fn kind_key(kind: &FieldKind) -> &'static str {
    match kind {
        FieldKind::Bool => "bool",
        FieldKind::Int { .. } => "int",
        FieldKind::Float { .. } => "float",
        FieldKind::String => "string",
        FieldKind::Text => "text",
        FieldKind::Path => "path",
        FieldKind::Enum(_) => "enum",
        FieldKind::StringArray => "stringArray",
        FieldKind::Object => "object",
        FieldKind::Rect => "rect",
        FieldKind::Point => "point",
        FieldKind::Secret => "secret",
        FieldKind::ModelName => "modelName",
    }
}

fn source_key(source: ConfigSource) -> &'static str {
    match source {
        ConfigSource::Db => "db",
        ConfigSource::Bootstrap => "bootstrap",
    }
}

/// 默认值树：内置默认配置；`playback.audio_cache` 默认是 null，其 6 个子字段
/// 用「启用后」的样例值补全（与 Web「启用」预填、`audio_cache_defaults_match_runtime_values`
/// 是同一份数据），否则快照里 6 条路径只能是 `<absent>`，看不出默认值漂移。
fn default_tree() -> Value {
    let mut tree = default_config_json();
    let sample = default_config_json_with_audio_cache();
    let sample_cache = sample
        .get("playback")
        .and_then(|playback| playback.get("audio_cache"))
        .cloned()
        .unwrap_or(Value::Null);
    if let Some(playback) = tree.get_mut("playback").and_then(Value::as_object_mut) {
        playback.insert("audio_cache".to_string(), sample_cache);
    }
    tree
}

fn lookup<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .try_fold(root, |current, segment| current.as_object()?.get(segment))
}

/// 默认值摘要：敏感字段一律 `<secret>`；超长字符串折叠成 `<string len=N>`。
/// 其余按 JSON 原样保留（对象/数组递归，保证结构变化也能被 diff 到）。
fn summarize(value: &Value, secret: bool) -> Value {
    if secret {
        return json!("<secret>");
    }
    match value {
        Value::Null => json!("<null>"),
        Value::String(text) if text.chars().count() > MAX_INLINE_STRING_CHARS => {
            json!(format!("<string len={}>", text.chars().count()))
        }
        Value::Array(items) if items.len() > MAX_INLINE_ARRAY_ITEMS => {
            json!(format!("<array len={}>", items.len()))
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| summarize(item, false)).collect())
        }
        Value::Object(map) if map.len() > MAX_INLINE_OBJECT_KEYS => {
            json!(format!("<object keys={}>", map.len()))
        }
        Value::Object(map) => {
            let mut summarized = Map::new();
            for (key, child) in map {
                summarized.insert(key.clone(), summarize(child, false));
            }
            Value::Object(summarized)
        }
        other => other.clone(),
    }
}

/// 生成快照 JSON（确定性：无 HashMap 顺序、无时间戳）。
fn render_snapshot() -> Value {
    let sections = config_sections();
    let defaults = default_tree();

    let mut effect_counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut kind_counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut source_counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut live_paths: Vec<String> = Vec::new();
    let mut secret_paths: Vec<String> = Vec::new();
    let mut section_values: Vec<Value> = Vec::new();
    let mut field_count = 0usize;

    for section in &sections {
        let mut field_values: Vec<Value> = Vec::new();
        for field in &section.fields {
            field_count += 1;
            *effect_counts.entry(effect_key(field.effect)).or_default() += 1;
            *kind_counts.entry(kind_key(&field.kind)).or_default() += 1;
            *source_counts.entry(source_key(field.source)).or_default() += 1;
            if field.effect == Effect::Live {
                live_paths.push(field.path.clone());
            }
            let secret = matches!(field.kind, FieldKind::Secret)
                || SECRET_PATHS.contains(&field.path.as_str());
            if secret {
                secret_paths.push(field.path.clone());
            }
            let default = lookup(&defaults, &field.path)
                .map(|value| summarize(value, secret))
                .unwrap_or_else(|| json!("<absent>"));
            field_values.push(json!({
                "path": field.path,
                "effect": field.effect,
                "source": source_key(field.source),
                "kind": kind_value(&field.kind),
                "default": default,
                "nullable": field.nullable,
                "optionalParent": field.optional_parent,
                "group": field.group,
            }));
        }
        section_values.push(json!({
            "name": section.name,
            "label": section.label,
            "domain": section.domain,
            "order": section.order,
            "fields": field_values,
        }));
    }

    // 排序后的 Live 清单：既方便人读，也让「新增一条 Live」在 diff 里只出现一行。
    live_paths.sort();
    secret_paths.sort();

    json!({
        "schemaVersion": 1,
        "generatedBy": GENERATED_BY,
        "regenerateWith": "UPDATE_SCHEMA_SNAPSHOT=1 cargo test --lib --no-default-features -- config::schema::tests_snapshot",
        "totals": {
            "sections": sections.len(),
            "fields": field_count,
            "effects": effect_counts,
            "kinds": kind_counts,
            "sources": source_counts,
        },
        "livePaths": live_paths,
        "secretPaths": secret_paths,
        "sections": section_values,
    })
}

/// 把多行 JSON 从第二行起统一加缩进（`"totals"` 这类小对象保持 pretty，方便人读）。
fn indent_tail(text: &str, spaces: usize) -> String {
    let pad = " ".repeat(spaces);
    let mut lines = text.lines();
    let mut out = String::new();
    if let Some(first) = lines.next() {
        out.push_str(first);
    }
    for line in lines {
        out.push('\n');
        out.push_str(&pad);
        out.push_str(line);
    }
    out
}

fn push_string_list(out: &mut String, key: &str, value: &Value) {
    let items = value.as_array().expect("清单必须是数组");
    out.push_str(&format!("  \"{key}\": [\n"));
    for (index, item) in items.iter().enumerate() {
        let comma = if index + 1 == items.len() { "" } else { "," };
        out.push_str(&format!(
            "    {}{comma}\n",
            serde_json::to_string(item).expect("清单项必须可序列化")
        ));
    }
    out.push_str("  ],\n");
}

/// 单条字段的稳定文本：键顺序固定为 `path / effect / source / kind / default /
/// nullable / optionalParent / group`（`serde_json::Map` 是 BTreeMap，直接用
/// `to_string` 会按字母序排，读起来主次颠倒，也不利于 review diff）。
fn field_line(field: &Value) -> String {
    let part = |key: &str| {
        serde_json::to_string(&field[key])
            .unwrap_or_else(|error| panic!("字段 {key} 序列化失败：{error}"))
    };
    format!(
        "{{\"path\":{},\"effect\":{},\"source\":{},\"kind\":{},\"default\":{},\"nullable\":{},\"optionalParent\":{},\"group\":{}}}",
        part("path"),
        part("effect"),
        part("source"),
        part("kind"),
        part("default"),
        part("nullable"),
        part("optionalParent"),
        part("group"),
    )
}

/// 快照文本：**一条字段一行**。
///
/// 不用 `to_string_pretty` 的整棵 pretty 树：那样一个字段要占 12 行，
/// 399 个字段就是 6,800 行，review 时一行改动会淹没在缩进里。这里让
/// 「改了一个字段 → diff 恰好一行」，同时保留 JSON 可解析性（有人证测试）。
fn render_snapshot_text() -> String {
    let snapshot = render_snapshot();
    let mut out = String::from("{\n");
    out.push_str(&format!(
        "  \"schemaVersion\": {},\n",
        serde_json::to_string(&snapshot["schemaVersion"]).expect("版本号可序列化")
    ));
    out.push_str(&format!(
        "  \"generatedBy\": {},\n",
        serde_json::to_string(&snapshot["generatedBy"]).expect("生成者字符串")
    ));
    out.push_str(&format!(
        "  \"regenerateWith\": {},\n",
        serde_json::to_string(&snapshot["regenerateWith"]).expect("重生成命令字符串")
    ));
    out.push_str(&format!(
        "  \"totals\": {},\n",
        indent_tail(
            &serde_json::to_string_pretty(&snapshot["totals"]).expect("totals 可序列化"),
            2
        )
    ));
    push_string_list(&mut out, "livePaths", &snapshot["livePaths"]);
    push_string_list(&mut out, "secretPaths", &snapshot["secretPaths"]);

    let sections = snapshot["sections"]
        .as_array()
        .expect("sections 必须是数组");
    out.push_str("  \"sections\": [\n");
    for (section_index, section) in sections.iter().enumerate() {
        out.push_str("    {\n");
        out.push_str(&format!(
            "      \"name\": {},\n",
            serde_json::to_string(&section["name"]).expect("段名")
        ));
        out.push_str(&format!(
            "      \"label\": {},\n",
            serde_json::to_string(&section["label"]).expect("段中文名")
        ));
        out.push_str(&format!(
            "      \"domain\": {},\n",
            serde_json::to_string(&section["domain"]).expect("域")
        ));
        out.push_str(&format!(
            "      \"order\": {},\n",
            serde_json::to_string(&section["order"]).expect("顺序")
        ));
        out.push_str("      \"fields\": [\n");
        let fields = section["fields"].as_array().expect("fields 必须是数组");
        for (field_index, field) in fields.iter().enumerate() {
            let comma = if field_index + 1 == fields.len() {
                ""
            } else {
                ","
            };
            out.push_str(&format!("        {}{comma}\n", field_line(field)));
        }
        out.push_str("      ]\n");
        let section_comma = if section_index + 1 == sections.len() {
            ""
        } else {
            ","
        };
        out.push_str(&format!("    }}{section_comma}\n"));
    }
    out.push_str("  ]\n}\n");
    out
}

/// 首个差异行 + 上下文，失败信息直接告诉 review 者「哪一条漂了」。
fn first_difference(expected: &str, actual: &str) -> String {
    let expected_lines: Vec<&str> = expected.lines().collect();
    let actual_lines: Vec<&str> = actual.lines().collect();
    for (index, (expected_line, actual_line)) in
        expected_lines.iter().zip(actual_lines.iter()).enumerate()
    {
        if expected_line != actual_line {
            let start = index.saturating_sub(3);
            let end = (index + 4).min(expected_lines.len().max(actual_lines.len()));
            let mut context = String::new();
            for line in start..end {
                context.push_str(&format!(
                    "  行 {:>4} | 快照: {}\n  行 {:>4} | 生成: {}\n",
                    line + 1,
                    expected_lines.get(line).copied().unwrap_or("<缺失>"),
                    line + 1,
                    actual_lines.get(line).copied().unwrap_or("<缺失>"),
                ));
            }
            return format!("首个差异在第 {} 行：\n{context}", index + 1);
        }
    }
    format!(
        "前缀一致，但行数不同：快照 {} 行，生成 {} 行",
        expected_lines.len(),
        actual_lines.len()
    )
}

/// 快照必须与当前 schema 完全一致；`UPDATE_SCHEMA_SNAPSHOT=1` 时改为写盘。
#[test]
fn schema_snapshot_is_current() {
    let generated = render_snapshot_text();
    let path = snapshot_path();

    if std::env::var_os("UPDATE_SCHEMA_SNAPSHOT").is_some() {
        std::fs::write(&path, &generated).expect("写入 schema 快照失败");
        eprintln!("已重新生成 schema 快照：{}", path.display());
        return;
    }

    let current = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "读取 schema 快照失败（{}）：{error}\n用 `UPDATE_SCHEMA_SNAPSHOT=1 cargo test --lib --no-default-features -- config::schema::tests_snapshot` 生成",
            path.display()
        )
    });
    assert_eq!(
        current,
        generated,
        "schema 快照已过期（schema 发生漂移）。确认漂移有意后运行 \
         `UPDATE_SCHEMA_SNAPSHOT=1 cargo test --lib --no-default-features -- config::schema::tests_snapshot` 重新生成。\n{}",
        first_difference(&current, &generated)
    );
}

/// 快照资产必须是合法 JSON（一条字段一行的紧凑格式由本文件手工排版）。
#[test]
fn snapshot_text_is_valid_json() {
    let text = render_snapshot_text();
    let parsed: Value = serde_json::from_str(&text).expect("快照必须是合法 JSON");
    assert_eq!(parsed["totals"]["sections"], json!(38));
    assert_eq!(parsed["totals"]["fields"], json!(399));
}

/// 快照规模与 effect 分布是 W0 的量化验收项：38 段 / 399 字段 / 121 Live。
/// 数字变化本身不是错误（新字段是允许的），但必须同步改这里和快照——
/// 目的是让「配置面在扩大」这件事在 review 里显式可见。
#[test]
fn snapshot_totals_record_the_quantified_schema_shape() {
    let snapshot = render_snapshot();
    let sections = snapshot["sections"]
        .as_array()
        .expect("sections 必须是数组");
    let fields: usize = sections
        .iter()
        .map(|section| {
            section["fields"]
                .as_array()
                .expect("fields 必须是数组")
                .len()
        })
        .sum();
    assert_eq!(sections.len(), 38, "配置段数量变化必须显式确认");
    assert_eq!(fields, 399, "配置字段数量变化必须显式确认");
    assert_eq!(snapshot["totals"]["fields"], json!(fields));

    let live = snapshot["livePaths"]
        .as_array()
        .expect("livePaths 必须是数组");
    assert_eq!(live.len(), 121, "Live 字段数量变化必须显式确认");
    assert_eq!(
        snapshot["totals"]["effects"]["live"],
        json!(121),
        "effect 分布里的 Live 计数必须与 Live 清单一致"
    );
    let effects = snapshot["totals"]["effects"]
        .as_object()
        .expect("effects 必须是对象");
    let effect_total: usize = effects
        .values()
        .map(|count| count.as_u64().expect("计数必须是整数") as usize)
        .sum();
    assert_eq!(effect_total, fields, "effect 分布必须覆盖全部字段");

    // 每段都有唯一段名，避免快照里的段被同名覆盖。
    let names: std::collections::BTreeSet<&str> = sections
        .iter()
        .map(|section| section["name"].as_str().expect("段名必须是字符串"))
        .collect();
    assert_eq!(names.len(), sections.len(), "段名必须唯一");
}

/// 敏感字段的默认值不得出现在快照里：`SECRET_PATHS` 的默认值在快照中必须是
/// `"<secret>"`，且非空的默认凭据字符串不得在任何段出现。
#[test]
fn snapshot_never_leaks_secret_defaults() {
    let text = render_snapshot_text();
    let defaults = default_tree();
    let snapshot = render_snapshot();

    let mut asserted = 0usize;
    for section in snapshot["sections"].as_array().expect("sections") {
        for field in section["fields"].as_array().expect("fields") {
            let path = field["path"].as_str().expect("path");
            if SECRET_PATHS.contains(&path) || field["kind"]["type"] == json!("secret") {
                asserted += 1;
                assert_eq!(
                    field["default"],
                    json!("<secret>"),
                    "secret 字段 {path} 的默认值必须脱敏为 <secret>"
                );
                if let Some(Value::String(secret)) = lookup(&defaults, path) {
                    if !secret.is_empty() && secret.len() >= 4 {
                        assert!(
                            !text.contains(secret.as_str()),
                            "快照泄露了 {path} 的默认凭据"
                        );
                    }
                }
            }
        }
    }
    assert!(asserted > 0, "必须至少有一个 secret 字段被检查");
    assert_eq!(
        snapshot["secretPaths"]
            .as_array()
            .expect("secretPaths")
            .len(),
        SECRET_PATHS.len(),
        "secret 字段集合必须与 SECRET_PATHS 一致"
    );
}
