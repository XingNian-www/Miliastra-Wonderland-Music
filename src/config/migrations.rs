//! 配置 schema 迁移：把旧版本的 `config_sections` 平滑搬到当前结构。
//!
//! 与「断代重置」的分工：
//!
//! - 有迁移路径的旧版本（当前是 v3 → v4）走本模块：只改被改名/删除的键，
//!   **其余字段与用户设置原样保留**；
//! - 没有迁移路径的更老版本（v1、v2 及更早）仍由 [`crate::config::store`] 的
//!   `reset_previous_schema` 断代重置并留 `.bak` 备份。
//!
//! 迁移只处理 `config_sections` 里存的 JSON 段（http/logging 由启动引导注入，
//! 不落库；`state.playback_state_path` 由统一数据库路径注入）。
//!
//! 字段名以 [`super::schema::config_sections`] 与各段 struct **实际声明**为准，
//! 不在本模块重新维护一份结构体反射。
//!
//! # 幂等
//!
//! 改名只在「旧键存在且新键不存在」时搬值，删除只在旧键存在时删；未知键剪枝
//! 在第一次跑完后已无可剪对象。因此对同一份 sections 重复执行结果不变（语义
//! 相同，见 `migrate_sections_is_idempotent`）。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

use super::AppConfig;
use super::schema::config_sections;
use super::store::CONFIG_SCHEMA_VERSION;

/// 最早有迁移路径的版本。
///
/// 低于此版本的配置没有可用的键映射表（v1 → v2 的 AI 记忆三层改造、v2 → v3
/// 的字段改名都没有留下可逆映射），只能按既定策略断代重置。
const MIGRATION_START_VERSION: i64 = 3;

/// 最早可迁移版本（供日志说明迁移路径覆盖范围）。
pub(crate) const fn migration_start_version() -> i64 {
    MIGRATION_START_VERSION
}

/// 一次字段改名：把 `from` 的旧值搬到 `to`。
struct RenamedField {
    /// 顶层段名（`config_sections.section`）。
    section: &'static str,
    /// 段内点路径（最多两级，如 `templates`）。
    from: &'static str,
    /// 段内点路径。
    to: &'static str,
}

/// 一次字段删除：段 + 段内点路径。
type RemovedField = (&'static str, &'static str);

/// v3 → v4 的字段改名表。
///
/// 每个条目都对应本轮真实的结构体改动，不是按需求文档的字段名猜测：
/// `TemplateConfig.friend_confirm` → `confirm_button`（通用确认按钮，好友操作
/// 与启动流程共用）、`ModerationConfig.confirm_region` →
/// `confirm_button_region`。
const V3_TO_V4_RENAMES: &[RenamedField] = &[
    RenamedField {
        section: "templates",
        from: "friend_confirm",
        to: "confirm_button",
    },
    RenamedField {
        section: "moderation",
        from: "confirm_region",
        to: "confirm_button_region",
    },
];

/// v3 → v4 需要改名的**自由 map 内键**（段, map 点路径, 旧键, 新键）。
///
/// `templates.panel_downsample_factors` 的键是模板用途名，由
/// `TemplateConfig::validate` 按固定清单校验；不清掉旧名会让迁移后的配置在
/// `load_full` 阶段报「未知用途」。
const V3_TO_V4_MAP_KEY_RENAMES: &[(&str, &str, &str, &str)] = &[(
    "templates",
    "panel_downsample_factors",
    "friend_confirm",
    "confirm_button",
)];

/// v3 → v4 删除的字段：旧值在 v4 已没有任何消费者。
///
/// - `templates.paimon_menu_panel`：派蒙菜单锚点模板随旧版 UI 判定一起移除；
/// - `screen.paimon_menu_rect`：同上，搜索区一并移除；
/// - `startup.templates.wonderland_confirm`：千星确认改用共用的确认按钮模板；
/// - `startup.wonderland_confirm_region`：同上，区域改由
///   `moderation.confirm_button_region` 承担。
const V3_TO_V4_REMOVED: &[RemovedField] = &[
    ("templates", "paimon_menu_panel"),
    ("screen", "paimon_menu_rect"),
    ("startup", "templates.wonderland_confirm"),
    ("startup", "wonderland_confirm_region"),
];

/// 该版本是否还有迁移路径（能直接按字段映射表升到当前版本）。
///
/// 只有 [`MIGRATION_START_VERSION`] 到当前版本之间的旧版本才可迁移；更老的版本
/// 由调用方回退到断代重置。
pub(crate) fn has_migration_path(from_version: i64) -> bool {
    (MIGRATION_START_VERSION..CONFIG_SCHEMA_VERSION).contains(&from_version)
}

/// 从 `from_version` 逐步迁移到当前版本，直接改写入参。
///
/// 调用方负责事务与落库：本函数只做内存内的键改名/删除/未知键剪枝，出错时
/// 不留下部分结果（sections 可能已被改动，但调用方一定会丢弃事务）。
///
/// 迁移完成后会按当前结构反序列化一次：字段映射表与结构体脱节时在这里报错，
/// 而不是等下一次 `load_full` 才失败。
pub(crate) fn migrate_sections(
    from_version: i64,
    sections: &mut BTreeMap<String, Value>,
) -> Result<()> {
    if from_version == CONFIG_SCHEMA_VERSION {
        return Ok(());
    }
    if from_version > CONFIG_SCHEMA_VERSION {
        bail!(
            "配置 schema 版本 {from_version} 高于当前程序（{CONFIG_SCHEMA_VERSION}），无法向下迁移"
        );
    }
    if !has_migration_path(from_version) {
        bail!(
            "没有从配置 schema 版本 v{from_version} 到 v{CONFIG_SCHEMA_VERSION} 的迁移路径（最早可迁移版本为 v{MIGRATION_START_VERSION}）"
        );
    }
    let mut version = from_version;
    while version < CONFIG_SCHEMA_VERSION {
        match version {
            3 => migrate_v3_to_v4(sections)?,
            other => bail!("配置迁移缺少 v{other} → v{} 的实现", other + 1),
        }
        version += 1;
    }
    prune_unknown_keys(sections);
    fill_missing_defaults(sections);
    validate_migrated(sections)?;
    Ok(())
}

/// v3 → v4：改名、删除已移除字段。
fn migrate_v3_to_v4(sections: &mut BTreeMap<String, Value>) -> Result<()> {
    for rename in V3_TO_V4_RENAMES {
        let Some(section) = section_object(sections, rename.section) else {
            continue;
        };
        if peek_path(section, rename.to).is_some() {
            if remove_path(section, rename.from).is_some() {
                log::warn!(
                    "配置迁移 v3→v4：字段「{}.{}」与「{}.{}」同时存在，保留新字段的值，旧字段的值已丢弃",
                    rename.section,
                    rename.from,
                    rename.section,
                    rename.to
                );
            }
            continue;
        }
        let Some(value) = remove_path(section, rename.from) else {
            continue;
        };
        put_path(section, rename.to, value).with_context(|| {
            format!(
                "配置迁移 v3→v4：把字段「{}.{}」改名为「{}.{}」失败",
                rename.section, rename.from, rename.section, rename.to
            )
        })?;
        log::debug!(
            "配置迁移 v3→v4：字段「{}.{}」已改名为「{}.{}」，值已搬运",
            rename.section,
            rename.from,
            rename.section,
            rename.to
        );
    }
    for (section_name, map_path, from_key, to_key) in V3_TO_V4_MAP_KEY_RENAMES {
        let Some(section) = section_object(sections, section_name) else {
            continue;
        };
        let Some(map) = peek_path(section, map_path) else {
            continue;
        };
        let Some(factor) = map.as_object().and_then(|map| map.get(*from_key)).cloned() else {
            continue;
        };
        let Some(object) = section_object_at(section, map_path) else {
            continue;
        };
        object.remove(*from_key);
        if object.contains_key(*to_key) {
            log::warn!(
                "配置迁移 v3→v4：{section_name}.{map_path} 里「{from_key}」与「{to_key}」同时存在，保留新键，旧键已丢弃"
            );
        } else {
            object.insert((*to_key).to_string(), factor);
            log::debug!(
                "配置迁移 v3→v4：{section_name}.{map_path} 里的键「{from_key}」已改名为「{to_key}」"
            );
        }
    }
    for (section_name, path) in V3_TO_V4_REMOVED {
        let Some(section) = section_object(sections, section_name) else {
            continue;
        };
        if remove_path(section, path).is_some() {
            log::warn!(
                "配置迁移 v3→v4：字段「{section_name}.{path}」已移除（v4 不再使用该模板/区域），旧值已丢弃"
            );
        }
    }
    rewrite_renamed_template_paths(sections);
    Ok(())
}

/// 模板文件改名后，旧配置里的路径会指向已删除文件。
///
/// 实机事故：`templates.confirm_button` 仍指向 `friend-confirm.png`，文件在
/// v4 清理时已删除，确认弹窗探针每帧 open 失败，整条确认判据静默失效。
/// 这里只按**文件名**改写，保留用户自己的目录前缀。
fn rewrite_renamed_template_paths(sections: &mut BTreeMap<String, Value>) {
    const RENAMED: [(&str, &str); 2] = [
        ("friend-confirm.png", "confirm-button.png"),
        ("ui-primary-friend.png", "friend-panel.png"),
    ];
    let Some(templates) = sections.get_mut("templates").and_then(Value::as_object_mut) else {
        return;
    };
    for value in templates.values_mut() {
        let Some(raw) = value.as_str() else {
            continue;
        };
        let Some((old, new)) = RENAMED.iter().find(|(old, _)| raw.ends_with(old)) else {
            continue;
        };
        let replaced = format!("{}{new}", &raw[..raw.len() - old.len()]);
        log::warn!(
            "配置迁移 v3→v4：模板路径「{raw}」指向已删除文件，已改写为「{replaced}」"
        );
        *value = Value::String(replaced);
    }
}

/// 迁移后进行剪枝用的 schema 索引：当前结构声明的全部点路径。
struct SchemaIndex {
    /// 允许留在配置库里的顶层段名。
    ///
    /// 取 `AppConfig::default().to_db_value()` 的键，而不是 `config_sections()`
    /// 的段名：后者为了面板展示也声明了 `http`/`logging`，但这两段由启动引导
    /// 注入、从不落库；如果旧库里残留它们，`load_full` 会判成「字段已不存在」。
    sections: BTreeSet<String>,
    /// 全部字段点路径。
    paths: BTreeSet<String>,
    /// 有子字段声明的容器路径（如 `playback.audio_cache`）。
    containers: BTreeSet<String>,
}

impl SchemaIndex {
    fn current() -> &'static Self {
        static INDEX: OnceLock<SchemaIndex> = OnceLock::new();
        INDEX.get_or_init(|| {
            let sections = config_sections();
            let paths: BTreeSet<String> = sections
                .iter()
                .flat_map(|section| section.fields.iter())
                .map(|field| field.path.clone())
                .collect();
            // 容器 = 所有**中间层**点路径（如 `startup.templates`、`playback.audio_cache`）：
            // 它们自己不一定在 schema 里单独声明（`startup.templates` 就只有子字段），
            // 但必须继续向下递归，否则整棵子树会被当成未知键删掉。
            let containers: BTreeSet<String> = paths
                .iter()
                .flat_map(|path| {
                    let mut prefixes = Vec::new();
                    let mut current = path.as_str();
                    while let Some((head, _)) = current.rsplit_once('.') {
                        prefixes.push(head.to_string());
                        current = head;
                    }
                    prefixes
                })
                .collect();
            let sections = AppConfig::default()
                .to_db_value()
                .as_object()
                .map(|object| object.keys().cloned().collect())
                .unwrap_or_default();
            Self {
                sections,
                paths,
                containers,
            }
        })
    }
}

/// 剪掉当前结构不认识的键与段，并告警。
///
/// 只按 [`config_sections`] 的声明剪枝：`FieldKind::Object`（自由 map、数组、
/// 高级 JSON）与 `Rect`/`Point` 整体是叶，内部键一律保留；有子字段声明的容器
/// 才向下递归。这样既能清掉旧版本留下的死键（否则 `load_full` 会报「字段不
/// 存在」），又不会误删 `ai.extra_body`、`custom_workflows.workflows` 这类
/// 用户自由填写的内容。
fn prune_unknown_keys(sections: &mut BTreeMap<String, Value>) {
    let index = SchemaIndex::current();
    let mut removed = Vec::new();
    let mut removed_sections = Vec::new();
    for section in sections.keys().cloned().collect::<Vec<_>>() {
        if !index.sections.contains(&section) {
            sections.remove(&section);
            removed_sections.push(section);
            continue;
        }
        if let Some(object) = sections.get_mut(&section).and_then(Value::as_object_mut) {
            prune_object(object, &section, index, &mut removed);
        }
    }
    if !removed_sections.is_empty() {
        log::warn!(
            "配置迁移：已忽略当前程序不再声明的配置段「{}」，段内旧值不再生效",
            removed_sections.join("」「")
        );
    }
    if !removed.is_empty() {
        log::warn!(
            "配置迁移：已移除当前程序不再声明的 {} 个配置字段「{}」，这些旧值不再生效",
            removed.len(),
            removed.join("」「")
        );
    }
}

/// 递归剪枝一个段对象；`prefix` 是该对象在 schema 中的点路径前缀。
fn prune_object(
    object: &mut Map<String, Value>,
    prefix: &str,
    index: &SchemaIndex,
    removed: &mut Vec<String>,
) {
    for key in object.keys().cloned().collect::<Vec<_>>() {
        let path = format!("{prefix}.{key}");
        if index.containers.contains(&path) {
            if let Some(child) = object.get_mut(&key).and_then(Value::as_object_mut) {
                prune_object(child, &path, index, removed);
            }
            // 容器为 null / 数组 / 标量时保持原样，由迁移末尾的结构校验判定。
            continue;
        }
        if index.paths.contains(&path) {
            continue;
        }
        object.remove(&key);
        removed.push(path);
    }
}

/// 用当前内置默认值补齐**缺失**的字段（只补，不覆盖）。
///
/// 加载器允许「存储里缺字段」，但前提是那个字段有 serde 默认值；本轮新增的
/// `moderation.confirm_message_region`、`templates.confirm_button` 等没有默认值，
/// 旧库里必然缺失，不补齐会让迁移后的库在 `load_full` 阶段报 `missing field`。
///
/// 只沿 schema 声明的**容器**向下递归：自由 map（`ai.extra_body`、
/// `custom_workflows.templates`、`templates.panel_downsample_factors`）整体是叶，
/// 不会把默认键塞进用户已经清空的自由结构里。
fn fill_missing_defaults(sections: &mut BTreeMap<String, Value>) {
    let defaults = AppConfig::default().to_db_value();
    let Some(default_object) = defaults.as_object() else {
        return;
    };
    let index = SchemaIndex::current();
    for (section, default_value) in default_object {
        if !index.sections.contains(section) {
            continue;
        }
        match sections.get_mut(section) {
            Some(stored) => fill_object(stored, default_value, section, index),
            None => {
                sections.insert(section.clone(), default_value.clone());
            }
        }
    }
}

/// 递归位置：`stored` 与 `default` 都必须是对象，否则保持原样由结构校验报错。
fn fill_object(
    stored: &mut Value,
    default: &Value,
    prefix: &str,
    index: &SchemaIndex,
) {
    let (Some(stored), Some(default)) = (stored.as_object_mut(), default.as_object()) else {
        return;
    };
    for (key, default_value) in default {
        let path = format!("{prefix}.{key}");
        match stored.get_mut(key) {
            Some(existing) => {
                // 未声明子字段的自由结构整体是叶，不往里补默认键。
                if index.containers.contains(&path) {
                    fill_object(existing, default_value, &path, index);
                }
            }
            None => {
                stored.insert(key.clone(), default_value.clone());
            }
        }
    }
}

/// 迁移结果必须能按当前结构反序列化。
fn validate_migrated(sections: &BTreeMap<String, Value>) -> Result<()> {
    let value = Value::Object(sections.iter().map(|(k, v)| (k.clone(), v.clone())).collect());
    serde_json::from_value::<AppConfig>(value).context(
        "配置迁移后的内容无法按当前结构解析（字段改名/删除映射表与当前结构不一致，或旧配置本身损坏）",
    )?;
    Ok(())
}

/// 取段对象（段缺失或不是对象时返回 None，不视为错误）。
fn section_object<'a>(
    sections: &'a mut BTreeMap<String, Value>,
    section: &str,
) -> Option<&'a mut Map<String, Value>> {
    sections.get_mut(section).and_then(Value::as_object_mut)
}

/// 读取段内点路径的值（不改动）。
fn peek_path<'a>(object: &'a Map<String, Value>, path: &str) -> Option<&'a Value> {
    match path.split_once('.') {
        Some((head, tail)) => object.get(head)?.as_object()?.get(tail),
        None => object.get(path),
    }
}

/// 删除段内点路径的值并返回旧值；路径不存在返回 None。
fn remove_path(object: &mut Map<String, Value>, path: &str) -> Option<Value> {
    match path.split_once('.') {
        Some((head, tail)) => object.get_mut(head)?.as_object_mut()?.remove(tail),
        None => object.remove(path),
    }
}

/// 取段内某点路径对应的对象（单层点路径；中间节点必须是对象）。
fn section_object_at<'a>(
    object: &'a mut Map<String, Value>,
    path: &str,
) -> Option<&'a mut Map<String, Value>> {
    match path.split_once('.') {
        Some((head, tail)) => object
            .get_mut(head)?
            .as_object_mut()?
            .get_mut(tail)?
            .as_object_mut(),
        None => object.get_mut(path)?.as_object_mut(),
    }
}

/// 写入段内点路径；中间节点缺失时创建对象，存在但不是对象时失败。
fn put_path(object: &mut Map<String, Value>, path: &str, value: Value) -> Result<()> {
    match path.split_once('.') {
        Some((head, tail)) => {
            let child = object
                .entry(head.to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            let child = child
                .as_object_mut()
                .with_context(|| format!("配置字段 {} 的父节点不是对象", head))?;
            put_path(child, tail, value)
        }
        None => {
            object.insert(path.to_string(), value);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 取某个段的可变对象（测试里替代 BTreeMap 的下标赋值）。
    fn seg_mut<'a>(
        sections: &'a mut BTreeMap<String, Value>,
        name: &str,
    ) -> &'a mut Map<String, Value> {
        sections
            .get_mut(name)
            .and_then(Value::as_object_mut)
            .unwrap_or_else(|| panic!("段 {name} 必须存在且是对象"))
    }

    /// 从当前默认配置构造一份「v3 视图」：把本轮改名/删除的字段还原成 v3 的键，
    /// 用来验证真实默认配置经迁移后仍是当前结构。
    fn v3_view() -> BTreeMap<String, Value> {
        let value = AppConfig::default().to_db_value();
        let mut sections: BTreeMap<String, Value> = value
            .as_object()
            .expect("默认配置序列化为对象")
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        // 改名字段还原成 v3 的旧键。
        {
            let templates = seg_mut(&mut sections, "templates");
            let value = templates.remove("confirm_button").unwrap();
            templates.insert("friend_confirm".into(), value);
            templates.insert("paimon_menu_panel".into(), json!("deps/assets/ui-paimon-menu-panel.png"));
        }
        {
            let moderation = seg_mut(&mut sections, "moderation");
            let value = moderation.remove("confirm_button_region").unwrap();
            moderation.insert("confirm_region".into(), value);
        }
        {
            let screen = seg_mut(&mut sections, "screen");
            screen.insert(
                "paimon_menu_rect".into(),
                json!({"x": 152, "y": 358, "width": 92, "height": 92}),
            );
        }
        {
            let startup = seg_mut(&mut sections, "startup");
            startup.insert(
                "wonderland_confirm_region".into(),
                json!({"x": 900, "y": 650, "width": 500, "height": 250}),
            );
            startup["templates"]
                .as_object_mut()
                .unwrap()
                .insert(
                    "wonderland_confirm".into(),
                    json!("deps/assets/startup-wonderland-confirm.png"),
                );
        }
        sections
    }

    #[test]
    fn migration_path_covers_only_versions_with_mapping_tables() {
        assert_eq!(CONFIG_SCHEMA_VERSION, 4, "本轮 v3 → v4 的版本号");
        assert!(has_migration_path(3));
        assert!(!has_migration_path(2), "v2 没有映射表，必须回退断代重置");
        assert!(!has_migration_path(1));
        assert!(!has_migration_path(CONFIG_SCHEMA_VERSION));
        assert!(!has_migration_path(CONFIG_SCHEMA_VERSION + 1));
    }

    #[test]
    fn v3_sections_are_renamed_removed_and_unrelated_fields_kept() {
        let mut sections = v3_view();
        // 用户设置必须跨迁移保留。
        seg_mut(&mut sections, "queue").insert("max_size".into(), json!(23));
        seg_mut(&mut sections, "ai").insert("api_key".into(), json!("old-secret"));
        let old_confirm = sections["templates"]["friend_confirm"].clone();
        let old_region = sections["moderation"]["confirm_region"].clone();

        migrate_sections(3, &mut sections).unwrap();

        assert_eq!(
            sections["templates"]["confirm_button"], old_confirm,
            "旧键的值必须搬到新键"
        );
        assert_eq!(
            sections["moderation"]["confirm_button_region"], old_region,
            "旧键的值必须搬到新键"
        );
        for (section, key) in [
            ("templates", "friend_confirm"),
            ("templates", "paimon_menu_panel"),
            ("screen", "paimon_menu_rect"),
            ("startup", "wonderland_confirm_region"),
        ] {
            assert!(
                sections[section].get(key).is_none(),
                "{section}.{key} 迁移后必须消失"
            );
        }
        assert!(
            sections["startup"]["templates"]
                .get("wonderland_confirm")
                .is_none(),
            "startup.templates.wonderland_confirm 迁移后必须消失"
        );
        assert_eq!(sections["queue"]["max_size"], json!(23), "无关字段必须保留");
        assert_eq!(sections["ai"]["api_key"], json!("old-secret"));
        // 迁移结果必须能被当前结构接受。
        validate_migrated(&sections).unwrap();
    }

    #[test]
    fn template_paths_pointing_to_deleted_files_are_rewritten() {
        let mut sections = v3_view();
        seg_mut(&mut sections, "templates").insert(
            "confirm_button".into(),
            json!("custom/extra/friend-confirm.png"),
        );
        seg_mut(&mut sections, "templates").insert(
            "friend_panel".into(),
            json!("deps/assets/ui-primary-friend.png"),
        );

        migrate_sections(3, &mut sections).unwrap();

        assert_eq!(
            sections["templates"]["confirm_button"],
            json!("custom/extra/confirm-button.png"),
            "旧确认模板路径必须改到新文件名，否则实机 open 失败"
        );
        assert_eq!(
            sections["templates"]["friend_panel"],
            json!("deps/assets/friend-panel.png")
        );
        validate_migrated(&sections).unwrap();
    }

    #[test]
    fn old_and_new_keys_together_keep_the_new_value() {
        let mut sections = v3_view();
        seg_mut(&mut sections, "templates").insert("confirm_button".into(), json!("custom/新确认.png"));
        seg_mut(&mut sections, "moderation").insert(
            "confirm_button_region".into(),
            json!({"x": 1, "y": 2, "width": 3, "height": 4}),
        );

        migrate_sections(3, &mut sections).unwrap();

        assert_eq!(
            sections["templates"]["confirm_button"],
            json!("custom/新确认.png")
        );
        assert_eq!(
            sections["moderation"]["confirm_button_region"],
            json!({"x": 1, "y": 2, "width": 3, "height": 4})
        );
        assert!(sections["templates"].get("friend_confirm").is_none());
        assert!(sections["moderation"].get("confirm_region").is_none());
    }

    #[test]
    fn migrate_sections_is_idempotent() {
        let mut sections = v3_view();
        migrate_sections(3, &mut sections).unwrap();
        let once = sections.clone();
        // 已迁移的 sections 再跑一遍（等价于版本号没更新但重复执行）。
        migrate_sections(3, &mut sections).unwrap();
        assert_eq!(sections, once, "重复迁移不得改变语义");
    }

    #[test]
    fn unknown_keys_and_sections_are_pruned_without_panicking() {
        let mut sections = v3_view();
        // 旧版本遗留的死键与整段未知段。
        sections.insert("qianxing_special_event".into(), json!({"template": "x.png"}));
        seg_mut(&mut sections, "templates")
            .insert("friend_more_settings_actions".into(), json!({"a": 1}));
        seg_mut(&mut sections, "moderation").insert(
            "more_settings_actions_region".into(),
            json!({"x": 0, "y": 0, "width": 1, "height": 1}),
        );
        seg_mut(&mut sections, "templates")
            .insert("invite_add_friend".into(), json!("deps/assets/old.png"));
        // 自由 map / 自由结构必须原样保留。
        seg_mut(&mut sections, "templates")
            .insert("panel_downsample_factors".into(), json!({"friend_panel": 1}));
        seg_mut(&mut sections, "ai").insert("extra_body".into(), json!({"top_k": 5}));
        // 自由 map：键名由用户决定，剪枝不得动它们。
        seg_mut(&mut sections, "custom_workflows")
            .get_mut("templates")
            .and_then(Value::as_object_mut)
            .unwrap()
            .insert("我的自定义流程".into(), json!("deps/assets/custom.png"));

        migrate_sections(3, &mut sections).unwrap();

        assert!(!sections.contains_key("qianxing_special_event"));
        assert!(
            sections["templates"]
                .get("friend_more_settings_actions")
                .is_none()
        );
        assert!(
            sections["moderation"]
                .get("more_settings_actions_region")
                .is_none()
        );
        assert!(sections["templates"].get("invite_add_friend").is_none());
        assert_eq!(
            sections["templates"]["panel_downsample_factors"],
            json!({"friend_panel": 1})
        );
        assert_eq!(sections["ai"]["extra_body"], json!({"top_k": 5}));
        assert_eq!(
            sections["custom_workflows"]["templates"]["我的自定义流程"],
            json!("deps/assets/custom.png")
        );
        validate_migrated(&sections).unwrap();
    }

    #[test]
    fn downsample_factor_key_is_renamed_so_validation_still_passes() {
        let mut sections = v3_view();
        seg_mut(&mut sections, "templates").insert(
            "panel_downsample_factors".into(),
            json!({"friend_confirm": 4, "friend_panel": 1}),
        );

        migrate_sections(3, &mut sections).unwrap();

        let factors = &sections["templates"]["panel_downsample_factors"];
        assert_eq!(factors["confirm_button"], json!(4), "缩放下采样倍数要跟到新键");
        assert!(factors.get("friend_confirm").is_none());
        assert_eq!(factors["friend_panel"], json!(1), "其它用途的倍数保留");
        // 自由 map 的键名受 TemplateConfig::validate 的固定清单约束：迁移后
        // 必须能通过 load_full 的 validate（否则升级后启动就失败）。
        let mut config: AppConfig = serde_json::from_value(Value::Object(
            sections
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ))
        .expect("迁移结果必须可反序列化");
        // playback_state_path 由启动引导注入（不落库），校验前补上。
        config.state.playback_state_path = "deps/data/playback.sqlite3".into();
        config.validate().expect("迁移结果必须通过完整校验");
    }

    /// 真实的 v3 库还缺本轮**新增**字段（它们没有 serde 默认值，比如
    /// `moderation.confirm_message_region`）；迁移必须补齐默认值，否则
    /// `load_full` 会在启动时 `missing field`。
    #[test]
    fn fields_added_in_v4_are_backfilled_with_defaults() {
        let mut sections = v3_view();
        // 本轮新增字段：从 v3 视图里全部删掉，模拟真实旧库。
        for (section, key) in [
            ("moderation", "confirm_message_region"),
            ("screen", "secondary_chat_submit_rect"),
            ("screen", "qianxing_f2_invite_rect"),
            ("templates", "confirm_message_random_hall"),
            ("templates", "confirm_message_return_teyvat"),
            ("templates", "confirm_message_block"),
            ("templates", "secondary_chat_submit"),
            ("templates", "qianxing_f2_hall_list"),
            ("templates", "qianxing_f2_invite_player"),
            ("templates", "qianxing_exit_menu"),
            ("templates", "qianxing_map_label"),
            ("templates", "qianxing_profile_page_wishlist"),
            ("templates", "qianxing_profile_page_wardrobe"),
            ("templates", "teyvat_profile_adventure_rank"),
            ("templates", "teyvat_profile_world_level"),
            ("templates", "teyvat_profile_add_friend"),
            ("templates", "qianxing_profile_domain_level"),
            ("templates", "qianxing_profile_add_friend"),
            ("templates", "qianxing_profile_view_profile"),
        ] {
            if let Some(object) = sections.get_mut(section).and_then(Value::as_object_mut) {
                object.remove(key);
            }
        }

        migrate_sections(3, &mut sections).unwrap();

        // 缺的字段已用默认值补齐，且迁移结果能按当前结构解析。
        assert_eq!(
            sections["moderation"]["confirm_message_region"],
            AppConfig::default().to_db_value()["moderation"]["confirm_message_region"]
        );
        assert!(
            sections["templates"].get("secondary_chat_submit").is_some(),
            "新增模板字段必须补齐默认值"
        );
        validate_migrated(&sections).unwrap();
    }

    #[test]
    fn bootstrap_only_sections_are_dropped_from_the_database() {
        let mut sections = v3_view();
        // http/logging 由 config.yaml 注入，schema 为展示而声明，但从不落库；
        // 旧库若残留它们，留着重开后 load_full 会报「字段已不存在」。
        sections.insert(
            "http".into(),
            json!({"listen": "127.0.0.1:0", "access_token": "占位"}),
        );
        sections.insert("logging".into(), json!({"level": "info"}));

        migrate_sections(3, &mut sections).unwrap();

        assert!(!sections.contains_key("http"), "注入段不得留在配置库");
        assert!(!sections.contains_key("logging"));
    }

    #[test]
    fn unknown_section_values_of_wrong_type_do_not_panic() {
        let mut sections = v3_view();
        // 结构损坏的段（不是对象）：既不能 panics，也不能当成功。
        sections.insert("queue".into(), json!("not-an-object"));
        let error = migrate_sections(3, &mut sections).unwrap_err();
        assert!(
            error.to_string().contains("无法按当前结构解析"),
            "损坏段必须以迁移失败报错: {error:#}"
        );
    }

    #[test]
    fn older_versions_without_a_path_are_rejected() {
        let mut sections = v3_view();
        let error = migrate_sections(2, &mut sections).unwrap_err();
        assert!(error.to_string().contains("没有从配置 schema 版本 v2"), "{error:#}");
        // 版本相等是 no-op，不报错。
        migrate_sections(CONFIG_SCHEMA_VERSION, &mut sections).unwrap();
        // 更高版本明确拒绝。
        assert!(migrate_sections(CONFIG_SCHEMA_VERSION + 1, &mut sections).is_err());
    }

    #[test]
    fn missing_old_keys_are_tolerated_and_backfilled() {
        // 旧键被手工清理过（friend_confirm 缺失）：不 panic，也不把迁移搞崩；
        // 新键用当前默认值补齐（`TemplateConfig.confirm_button` 没有 serde 默认值）。
        let mut sections = v3_view();
        seg_mut(&mut sections, "templates").remove("friend_confirm");
        migrate_sections(3, &mut sections).expect("旧键缺失不应让迁移失败");
        assert_eq!(
            sections["templates"]["confirm_button"],
            AppConfig::default().to_db_value()["templates"]["confirm_button"]
        );
        validate_migrated(&sections).unwrap();
    }
}
