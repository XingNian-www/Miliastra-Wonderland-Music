//! 启动容错（W5/D7）：把 4 类**用户容器**里的坏条目隔离进 `config_quarantine`。
//!
//! 背景：面板保存路径会先 `validate_candidate`，正常操作写不进坏数据；坏数据只
//! 可能来自手改 sqlite/`.bak` 回灌、迁移缺陷、后台写路径或未来的导入功能——
//! 恰恰是高级用户与「出事之后」最需要容错的路径。旧行为是 14 类自定义面共用一次
//! 全量 `validate()`，一条坏数据让整个进程起不来。
//!
//! # 隔离什么
//!
//! 只有 [`super::codec::LoadMode::Lenient`] 会调用这里，且只处理 4 类用户容器：
//!
//! | 容器 | 坏条目 | 隔离方式 |
//! |---|---|---|
//! | `identity.mappings` | 重复 UID / 重复备注 / 重复映射 ID / 多个 owner | 该条映射移除（UID 不再由它授权） |
//! | `command_access.overrides` | 未知命令 ID / 低于最低权限 | 该条覆盖移除 |
//! | `custom_workflows.workflows` | 未知 step / 参数非法 | 该 workflow `enabled=false`（保留用户脚本原文） |
//! | `templates.panel_downsample_factors` | 未知用途 / 倍数越界 | 该键移除 |
//!
//! 结构错误（反序列化、`validate_persisted_shape`）与启动关键项
//! （`database_path` / `executable_root`）**在两种模式下都严格**：它们在这层之前
//! 就已经失败，这里看不到。
//!
//! # 冲突裁决
//!
//! 逐条「试加入」：按原始顺序把条目加进已保留集合并重新校验，失败的那条被隔离、
//! 回退。因此同一 UID 出现在两条映射里时，**靠前的一条保留、靠后的被隔离**
//! （确定性、不静默改权限：靠前条目原有的权限不变，靠后条目不能凭重复 UID 获得
//! 授权）。同一条映射内部重复绑定同一个账号时整条映射被隔离——那条 UID 不再由
//! 任何人授权。
//!
//! # 隐私
//!
//! `original_json` 可能含用户数据；写库前统一走 [`mask_original_json`]，复用
//! [`crate::config::store::codec::mask_secrets`] 的 [`SECRET_PATHS`] 掩码，
//! 把 `ai.api_key` 这类密钥替换为 `••••••`。四个容器本身不含 secret 字段，
//! 但迁移剪枝可能剪到含密钥的旧段/旧键，所以统一走同一层掩码。

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::{Map, Value};

use super::super::AppConfig;
use super::codec;
use super::sqlite::now_ms;

/// 一条待写入 quarantine 的坏条目（尚未掩码、尚未落库）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct QuarantineRecord {
    /// 所属顶层段名。
    pub(crate) section: String,
    /// 元素路径（点路径；数组元素带 `[index]`）。
    pub(crate) element_ref: String,
    /// 隔离原因（校验错误原文或迁移剪枝说明）。
    pub(crate) reason: String,
    /// 被隔离的原始 JSON（写库前掩码）。
    pub(crate) original_json: Value,
}

/// quarantine 表的一行（只读投影，供测试与后续「数据维护」面板使用）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuarantineEntry {
    pub id: i64,
    pub section: String,
    pub element_ref: String,
    pub reason: String,
    /// 掩码后的原始 JSON 文本。
    pub original_json: String,
    pub created_at_ms: u64,
}

/// 对 `config` 里 4 类用户容器做逐条隔离，返回被隔离的条目（不写库）。
///
/// 每个容器独立探测：探测前把**全部 4 类容器**清空再逐条加入，避免其它容器的
/// 坏条目污染本容器的判定；容器之外的校验错误不会被这里掩盖，调用方隔离后
/// 必须重新 `validate()`，仍有错误就按严格模式失败。
pub(super) fn isolate_user_container_errors(
    config: &mut AppConfig,
    stored: &Value,
) -> Vec<QuarantineRecord> {
    let mut records = Vec::new();
    isolate_identity(config, stored, &mut records);
    isolate_command_overrides(config, stored, &mut records);
    isolate_workflows(config, stored, &mut records);
    isolate_downsample_factors(config, stored, &mut records);
    records
}

/// 只把 4 类容器清空的探测基线：其余段保持原样。
///
/// 基线仍校验其它段；若基线失败，说明错误不在 4 类容器里，本容器不做任何
/// 隔离（避免把无辜条目写成 quarantine）。
fn probe_baseline(config: &AppConfig) -> Option<AppConfig> {
    let mut probe = config.clone();
    probe.identity.mappings.clear();
    probe.command_access.overrides.clear();
    probe.custom_workflows.workflows.clear();
    probe.templates.panel_downsample_factors.clear();
    probe.validate().is_ok().then_some(probe)
}

fn isolate_identity(config: &mut AppConfig, stored: &Value, records: &mut Vec<QuarantineRecord>) {
    let Some(base) = probe_baseline(config) else {
        return;
    };
    let mappings = std::mem::take(&mut config.identity.mappings);
    let mut kept = Vec::with_capacity(mappings.len());
    for (index, mapping) in mappings.into_iter().enumerate() {
        let mut probe = base.clone();
        let mut candidate = kept.clone();
        candidate.push(mapping.clone());
        probe.identity.mappings = candidate;
        match probe.validate() {
            Ok(()) => kept.push(mapping),
            Err(error) => records.push(QuarantineRecord {
                section: "identity".to_string(),
                element_ref: format!("identity.mappings[{index}]"),
                reason: error.to_string(),
                original_json: stored_element(
                    stored,
                    "identity",
                    "mappings",
                    StoredKey::Index(index),
                ),
            }),
        }
    }
    config.identity.mappings = kept;
}

fn isolate_command_overrides(
    config: &mut AppConfig,
    stored: &Value,
    records: &mut Vec<QuarantineRecord>,
) {
    let Some(base) = probe_baseline(config) else {
        return;
    };
    let overrides = std::mem::take(&mut config.command_access.overrides);
    let mut kept = std::collections::BTreeMap::new();
    for (id, policy) in overrides {
        let mut probe = base.clone();
        let mut candidate = kept.clone();
        candidate.insert(id.clone(), policy.clone());
        probe.command_access.overrides = candidate;
        match probe.validate() {
            Ok(()) => {
                kept.insert(id, policy);
            }
            Err(error) => {
                let element_ref = format!("command_access.overrides.{id}");
                records.push(QuarantineRecord {
                    section: "command_access".to_string(),
                    element_ref,
                    reason: error.to_string(),
                    original_json: stored_element(
                        stored,
                        "command_access",
                        "overrides",
                        StoredKey::Key(&id),
                    ),
                });
            }
        }
    }
    config.command_access.overrides = kept;
}

fn isolate_workflows(config: &mut AppConfig, stored: &Value, records: &mut Vec<QuarantineRecord>) {
    let Some(base) = probe_baseline(config) else {
        return;
    };
    let workflows = std::mem::take(&mut config.custom_workflows.workflows);
    let mut kept = Vec::with_capacity(workflows.len());
    for (index, workflow) in workflows.into_iter().enumerate() {
        // 已禁用的流程本来就不参与校验（也不会被调度），原样保留。
        if !workflow.enabled {
            kept.push(workflow);
            continue;
        }
        let mut probe = base.clone();
        let mut candidate: Vec<_> = kept
            .iter()
            .filter(|kept_workflow| kept_workflow.enabled)
            .cloned()
            .collect();
        candidate.push(workflow.clone());
        probe.custom_workflows.workflows = candidate;
        match probe.validate() {
            Ok(()) => kept.push(workflow),
            Err(error) => {
                records.push(QuarantineRecord {
                    section: "custom_workflows".to_string(),
                    element_ref: format!("custom_workflows.workflows[{index}]"),
                    // 校验错误不一定带流程名，补上名字便于面板定位到条目。
                    reason: format!("工作流「{}」：{error}", workflow.name.trim()),
                    original_json: stored_element(
                        stored,
                        "custom_workflows",
                        "workflows",
                        StoredKey::Index(index),
                    ),
                });
                // 隔离方式 = 禁用而不是删除：保留用户脚本原文，修好 step 后重新启用即可。
                let mut disabled = workflow;
                disabled.enabled = false;
                kept.push(disabled);
            }
        }
    }
    config.custom_workflows.workflows = kept;
}

fn isolate_downsample_factors(
    config: &mut AppConfig,
    stored: &Value,
    records: &mut Vec<QuarantineRecord>,
) {
    let Some(base) = probe_baseline(config) else {
        return;
    };
    let factors = std::mem::take(&mut config.templates.panel_downsample_factors);
    let mut kept = std::collections::BTreeMap::new();
    for (key, factor) in factors {
        let mut probe = base.clone();
        let mut candidate = kept.clone();
        candidate.insert(key.clone(), factor);
        probe.templates.panel_downsample_factors = candidate;
        match probe.validate() {
            Ok(()) => {
                kept.insert(key, factor);
            }
            Err(error) => {
                let element_ref = format!("templates.panel_downsample_factors.{key}");
                records.push(QuarantineRecord {
                    section: "templates".to_string(),
                    element_ref,
                    reason: error.to_string(),
                    original_json: stored_element(
                        stored,
                        "templates",
                        "panel_downsample_factors",
                        StoredKey::Key(&key),
                    ),
                });
            }
        }
    }
    config.templates.panel_downsample_factors = kept;
}

/// 从原始 sections 里取出元素原文（取不到时给 `null`，绝不因此让启动失败）。
enum StoredKey<'a> {
    Index(usize),
    Key(&'a str),
}

fn stored_element(stored: &Value, section: &str, container: &str, key: StoredKey<'_>) -> Value {
    let Some(container) = stored
        .get(section)
        .and_then(|section_value| section_value.get(container))
    else {
        return Value::Null;
    };
    match key {
        StoredKey::Index(index) => container.get(index).cloned().unwrap_or(Value::Null),
        StoredKey::Key(key) => container.get(key).cloned().unwrap_or(Value::Null),
    }
}

/// 写入隔离记录：同一 `(section, element_ref)` 只保留最新一条。
///
/// 反复以 Lenient 启动（用户没修）不会每次再堆一行；写库失败由调用方降级为
/// warn——quarantine 是“找回”用的附加信息，不能反过来阻断启动。
pub(super) fn store_records(
    connection: &Connection,
    records: &[QuarantineRecord],
) -> Result<usize> {
    if records.is_empty() {
        return Ok(0);
    }
    let timestamp = now_ms() as i64;
    let transaction = connection.unchecked_transaction()?;
    for record in records {
        let original =
            mask_original_json(&record.section, &record.element_ref, &record.original_json);
        transaction.execute(
            "DELETE FROM config_quarantine WHERE section = ?1 AND element_ref = ?2",
            rusqlite::params![record.section, record.element_ref],
        )?;
        transaction.execute(
            "INSERT INTO config_quarantine (section, element_ref, reason, original_json, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                record.section,
                record.element_ref,
                record.reason,
                serde_json::to_string(&original).context("序列化 quarantine 的 original_json 失败")?,
                timestamp
            ],
        )?;
    }
    transaction.commit()?;
    Ok(records.len())
}

/// 读取全部隔离记录（按 id 升序）。
pub(super) fn read_entries(connection: &Connection) -> Result<Vec<QuarantineEntry>> {
    let mut statement = connection.prepare(
        "SELECT id, section, element_ref, reason, original_json, created_at_ms
           FROM config_quarantine ORDER BY id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(QuarantineEntry {
            id: row.get(0)?,
            section: row.get(1)?,
            element_ref: row.get(2)?,
            reason: row.get(3)?,
            original_json: row.get(4)?,
            created_at_ms: u64::try_from(row.get::<_, i64>(5)?).unwrap_or(0),
        })
    })?;
    let mut entries = Vec::new();
    for row in rows {
        entries.push(row?);
    }
    Ok(entries)
}

/// 写库前对 `original_json` 做 secret 掩码（复用 `SECRET_PATHS` 口径）。
///
/// 三种形态：
/// 1. 元素是段内子路径（如 `ai.api_key`）：放回真实点路径后掩码；
/// 2. 元素就是整段（段被删除，如 `ai`）：以段名为前缀递归掩码，覆盖 `ai.api_key`；
/// 3. 自由容器元素（路径含 `[index]` 或 map 键）：容器本身不含 secret 字段，
///    仍跑一遍无前缀掩码兜底。
pub(super) fn mask_original_json(section: &str, element_ref: &str, value: &Value) -> Value {
    if !value.is_null()
        && let Some(rest) = element_ref
            .strip_prefix(section)
            .and_then(|rest| rest.strip_prefix('.'))
        && !rest.contains('[')
    {
        // 形态 1：把元素放回段内真实路径，交给 SECRET_PATHS 的绝对路径匹配。
        let mut root = Map::new();
        if codec::set_path(&mut root, element_ref, value.clone()) {
            let mut root_value = Value::Object(root);
            codec::mask_secrets(&mut root_value);
            if let Some(masked) = root_value
                .as_object()
                .and_then(|root| codec::get_path(root, element_ref))
            {
                return masked.clone();
            }
        }
    }
    let mut masked = value.clone();
    if element_ref == section {
        // 形态 2：整段被移除，段名就是路径前缀。
        codec::mask_secrets_inner(&mut masked, section);
    } else {
        // 形态 3：容器元素/无前缀形态，能掩多少算多少。
        codec::mask_secrets(&mut masked);
    }
    masked
}
