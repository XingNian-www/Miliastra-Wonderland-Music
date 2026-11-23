//! 配置段编解码：sections ↔ `AppConfig`、路径与密钥规范化、脱敏与差异。
//!
//! 这一层**不碰 SQL**：输入输出都是 `serde_json` 的段映射。四条不变量：
//!
//! - **存储里缺字段允许、多字段拒绝**（[`validate_persisted_shape`]）：缺字段是
//!   「程序新增了配置项」，serde 默认值补齐即可；多字段是字段被删或改名，静默
//!   丢弃会让用户丢设置，必须报错。
//! - **注入段不进库**：`http` / `logging` 由启动引导提供，
//!   `state.playback_state_path` 由统一数据库路径注入（[`merge_sections`]）。
//! - **路径只按 schema 声明规范化**：只认 `FieldKind::Path` 的字段，不猜普通字符串
//!   是路径（[`normalize_path_fields`]）。
//! - **密钥只按显式协议改写**：`null` / 空串 / 掩码保留旧值，
//!   `{"__clear__": true}` 才清除（[`apply_secret_clear_markers`] →
//!   [`retain_secret_fields`]，顺序不能反）。

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::OnceLock;

use anyhow::Result;
use serde_json::{Map, Value};

use crate::config::FieldKind;
use crate::config::config_sections;
use crate::config::normalize_native_path_string;

use super::super::AppConfig;
use super::super::BootstrapConfig;
use super::super::schema::SECRET_PATHS;
use super::types::{ConfigFieldError, ConfigFieldErrors};

/// secret 字段脱敏展示时使用的掩码字符串（Web 页面显示与表单回传值）。
/// 保存时提交值等于该掩码视为「未修改」，保留库中当前值，防止覆盖真实密钥。
pub const SECRET_MASK: &str = "••••••";

/// secret 字段清除标记的键名：提交值等于 JSON 对象 `{"__clear__": true}` 时
/// 表示**清除**该字段（写入空字符串，绕过保留规则）；Web 页面「清除」按钮使用。
pub const SECRET_CLEAR_MARKER: &str = "__clear__";

/// 配置库加载模式（W5 容错半，D7）。
///
/// - [`LoadMode::Strict`]：**默认**，任何反序列化/结构/业务校验失败都拒绝启动，
///   与重构前逐字节一致（回退方式 = 保持默认，不设开关）。
/// - [`LoadMode::Lenient`]：显式开启后，只隔离 4 类**用户容器**里的坏条目
///   （重复 UID 的身份映射、未知命令覆盖、未知 workflow step、未知模板倍数），
///   把它们写入 `config_quarantine` 并继续启动；结构错误与启动关键项
///   （`database_path` / `executable_root`）在两种模式下都严格。
///
/// **这是待用户批准的行为变更**（Lead 裁决 3）：默认关闭，只有显式设置
/// 环境变量 `MILIASTRA_CONFIG_LOAD_MODE=lenient` 才会生效；启动链仍然是
/// 「先 Strict，失败且开关开启才降级重试」，不是一上来就用宽松模式。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LoadMode {
    /// 严格（默认）：校验失败即拒绝启动。
    #[default]
    Strict,
    /// 宽松（显式开关）：隔离 4 类用户容器的坏条目后继续启动。
    Lenient,
}

impl LoadMode {
    /// 显式开关的环境变量名：值为 `lenient` 时启用，其余（含未设置）都是 `strict`。
    pub const ENV: &str = "MILIASTRA_CONFIG_LOAD_MODE";

    /// 从进程环境读取加载模式；未设置或无法识别一律回落到 [`LoadMode::Strict`]。
    pub fn from_env() -> Self {
        let value = std::env::var(Self::ENV).ok();
        Self::from_env_value(value.as_deref())
    }

    /// 纯解析（便于测试，不读进程环境）。
    ///
    /// 只认 `lenient`（大小写/首尾空白不敏感）；`strict`、空串、未设置都回落到
    /// 严格；无法识别的值告警后同样回落到严格——宽松模式必须是**显式且拼对**的。
    pub fn from_env_value(value: Option<&str>) -> Self {
        match value.map(str::trim).filter(|value| !value.is_empty()) {
            None => Self::Strict,
            Some(value) if value.eq_ignore_ascii_case("lenient") => Self::Lenient,
            Some(value) if value.eq_ignore_ascii_case("strict") => Self::Strict,
            Some(other) => {
                log::warn!(
                    "环境变量 {} 的值「{other}」无法识别（只支持 strict/lenient），按默认的 strict 处理",
                    Self::ENV
                );
                Self::Strict
            }
        }
    }

    /// 是否宽松模式（读起来比 `== LoadMode::Lenient` 更贴近调用点语义）。
    pub const fn is_lenient(self) -> bool {
        matches!(self, Self::Lenient)
    }
}

impl AppConfig {
    /// 序列化为数据库存储值：删除 http/logging 段（由启动引导提供）与
    /// state.playback_state_path（由统一数据库路径注入）。
    pub(crate) fn to_db_value(&self) -> Value {
        let mut value = serde_json::to_value(self).expect("AppConfig 序列化不应失败");
        let object = value
            .as_object_mut()
            .expect("AppConfig 序列化结果必须是对象");
        object.remove("http");
        object.remove("logging");
        if let Some(state) = object.get_mut("state").and_then(Value::as_object_mut) {
            state.remove("playback_state_path");
        }
        value
    }

    /// 从当前 schema 的完整持久化值组装配置；缺失或未知字段明确拒绝。
    /// 随后注入启动引导提供的 http/logging 与统一数据库路径，并解析相对路径
    /// （state.playback_state_path 已是绝对路径，再次解析保持不变）。
    pub(crate) fn from_db_value(
        value: Value,
        bootstrap: &BootstrapConfig,
        executable_root: &Path,
    ) -> Result<AppConfig> {
        let mut config: AppConfig = serde_json::from_value(value.clone())?;
        validate_persisted_shape(&value, &config.to_db_value(), "")?;
        config.state.playback_state_path = bootstrap.database_path.clone();
        config.http = bootstrap.http.clone();
        config.logging = bootstrap.logging.clone();
        config.resolve_runtime_paths(executable_root);
        config.playback.normalize_audio_cache_paths(executable_root);
        Ok(config)
    }
}

/// 比较反序列化前后的键结构。
///
/// 允许**存储里缺字段**：那是「程序新增了配置项」，serde 已经用默认值补齐，
/// 属于向后兼容的升级。以前这里把缺字段也当错误，导致每加一个字段就要升
/// schema 版本、把用户整份配置断代重置——代价过大。
///
/// 仍然拒绝**存储里多出字段**：那是字段被删除或改名，旧值会被静默丢弃，
/// 必须让用户看见错误而不是悄悄丢失设置。
///
/// 实际配置值重新序列化后保留自由 map、可空对象和数组条目的真实结构。
///
/// **分段收集**（W5/D9）：旧实现遇到第一个多余键就 `bail`，一份配置里同时有多处
/// 字段被删/改名时用户只能一条一条改。现在整棵树遍历完再一次性返回，每条错误
/// 都带 [`ConfigFieldError::element_ref`]（段名 + 数组下标/对象键），足以定位到
/// 具体条目。注意这**不改变严格性**：只要有一条就仍然失败，只是错误更完整。
pub(super) fn validate_persisted_shape(
    stored: &Value,
    canonical: &Value,
    path: &str,
) -> Result<()> {
    let mut errors = Vec::new();
    collect_persisted_shape_errors(stored, canonical, path, &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ConfigFieldErrors { errors }.into())
    }
}

/// [`validate_persisted_shape`] 的递归收集实现：只记录错误，不提前返回。
fn collect_persisted_shape_errors(
    stored: &Value,
    canonical: &Value,
    path: &str,
    errors: &mut Vec<ConfigFieldError>,
) {
    match (stored, canonical) {
        (Value::Object(stored), Value::Object(canonical)) => {
            let field_name = |key: &String| {
                if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                }
            };
            // 存储里有、结构体里没有 = 字段被移除或改名。
            for key in stored.keys().filter(|key| !canonical.contains_key(*key)) {
                let field = field_name(key);
                errors.push(ConfigFieldError::from_field(
                    field.clone(),
                    format!("配置字段 {field} 已不存在（字段被删除或改名）"),
                ));
            }
            // 结构体里有、存储里没有 = 新增字段，默认值已生效，递归交给下层校验。
            for (key, canonical_value) in canonical {
                let Some(stored_value) = stored.get(key) else {
                    continue;
                };
                collect_persisted_shape_errors(
                    stored_value,
                    canonical_value,
                    &field_name(key),
                    errors,
                );
            }
        }
        (Value::Array(stored), Value::Array(canonical)) => {
            for (index, (stored, canonical)) in stored.iter().zip(canonical).enumerate() {
                collect_persisted_shape_errors(
                    stored,
                    canonical,
                    &format!("{path}[{index}]"),
                    errors,
                );
            }
        }
        _ => {}
    }
}
/// 合并候选 sections：当前与提交的并集，提交段整体覆盖。
/// 同时剔除由启动引导注入的 http/logging 段与 state.playback_state_path
/// （这些项不在库中持久化，写入与展示时都不出现）。
pub(super) fn merge_sections(
    current: &Map<String, Value>,
    submitted: &Map<String, Value>,
) -> Map<String, Value> {
    let mut merged = current.clone();
    for (section, value) in submitted {
        merged.insert(section.clone(), value.clone());
    }
    merged.remove("http");
    merged.remove("logging");
    if let Some(state) = merged.get_mut("state").and_then(Value::as_object_mut) {
        state.remove("playback_state_path");
    }
    merged
}

/// 按点路径读取嵌套值；中间节点必须是对象，路径不存在时返回 None。
pub(super) fn get_path<'a>(root: &'a Map<String, Value>, path: &str) -> Option<&'a Value> {
    let segments = path.split('.').collect::<Vec<_>>();
    let mut current = root;
    for (index, segment) in segments.iter().enumerate() {
        let value = current.get(*segment)?;
        if index == segments.len() - 1 {
            return Some(value);
        }
        current = value.as_object()?;
    }
    None
}

/// 按点路径写入嵌套值；中间节点不存在时创建对象。
/// 中间节点已存在但不是对象时视为数据损坏，直接失败。
pub(super) fn set_path(map: &mut Map<String, Value>, path: &str, value: Value) -> bool {
    let segments = path.split('.').collect::<Vec<_>>();
    if segments.is_empty() {
        return false;
    }
    let mut current = map;
    for segment in &segments[..segments.len() - 1] {
        let entry = current
            .entry((*segment).to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        let Some(object) = entry.as_object_mut() else {
            return false;
        };
        current = object;
    }
    current.insert(segments[segments.len() - 1].to_string(), value);
    true
}

/// schema 中声明为路径（`FieldKind::Path`）的字段点路径，进程内只计算一次。
pub(super) fn path_field_paths() -> &'static [String] {
    static PATHS: OnceLock<Vec<String>> = OnceLock::new();
    PATHS.get_or_init(|| {
        config_sections()
            .into_iter()
            .flat_map(|section| section.fields)
            .filter(|field| field.kind == FieldKind::Path)
            .map(|field| field.path)
            .collect()
    })
}

/// 规范化一段配置对象里的路径类字段（只认 schema 声明的 `FieldKind::Path`，
/// 不凭空猜任意字符串是路径）。
///
/// - 任何情况下都把 `/` 与 `\` 统一成平台原生分隔符（Windows 上是 `\`）；
/// - `relativize_under_root` 为真时，**位于 executable_root 之下**的绝对路径
///   回落为相对路径（与默认值 `deps/...` 语义一致，避免 Web 保存又把绝对路径
///   写回库）；root 之外的绝对路径原样保留。
///
/// 覆盖范围：schema 的 43 个 `FieldKind::Path` 字段，含启动引导注入的
/// `logging.dir`（http 段没有路径字段）。bootstrap 注入值也走同一套规范化。
///
/// 已知限制（仅影响显示，不损坏数据）：`Path::strip_prefix` 在 Windows 上是
/// 大小写敏感的路径比较；若用户实际盘符/目录大小写与 executable_root 不一致
/// （如 `C:\Users\XingNian` vs `c:\users\...`），回落可能失败，此时继续
/// 显示规范化后的绝对路径；下一次保存仍得到纯原生分隔符，不产生混合分隔符。
pub(super) fn normalize_path_fields(
    sections: &mut Map<String, Value>,
    executable_root: &Path,
    relativize_under_root: bool,
) {
    for path in path_field_paths() {
        let Some(current) = get_path(sections, path)
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            continue;
        };
        if current.is_empty() {
            continue;
        }
        let normalized = if relativize_under_root {
            let candidate = Path::new(&current);
            if candidate.is_absolute() {
                match candidate.strip_prefix(executable_root) {
                    Ok(relative) => relative
                        .to_string_lossy()
                        .replace(std::path::MAIN_SEPARATOR, "/"),
                    Err(_) => normalize_native_path_string(&current),
                }
            } else {
                normalize_native_path_string(&current)
            }
        } else {
            normalize_native_path_string(&current)
        };
        if normalized != current {
            let _ = set_path(sections, path, Value::String(normalized));
        }
    }
}

/// 只规范化**指定段**里的路径字段（定向写入用）。
///
/// 与 [`normalize_path_fields`] 的区别：未提交的段根本不参与遍历，因此后台定向
/// 写入（如自动建身份只写 `identity`）不会顺手把其它段的相对路径重写成绝对路径，
/// 也不会改动它们的分隔符。
pub(super) fn normalize_section_path_fields(
    section: &str,
    value: &mut Value,
    executable_root: &Path,
) {
    // 复用整段实现：把单段包成只含该段的根映射，路径前缀自然只命中本段。
    let mut wrapper = Map::new();
    wrapper.insert(section.to_string(), value.clone());
    normalize_path_fields(&mut wrapper, executable_root, false);
    if let Some(normalized) = wrapper.remove(section) {
        *value = normalized;
    }
}

/// 应用 secret 清除标记：候选值等于 JSON 对象 `{"__clear__": true}`
/// （[`SECRET_CLEAR_MARKER`]）时，把 merged 中该路径写为空字符串，并记入
/// `forced_cleared` 集合（[`retain_secret_fields`] 跳过该路径，防止保留规则
/// 把清除请求还原成当前值）。必须在 `retain_secret_fields` 之前调用。
pub(super) fn apply_secret_clear_markers(
    merged: &mut Map<String, Value>,
    forced_cleared: &mut BTreeSet<&'static str>,
) {
    for field in SECRET_PATHS {
        let is_clear_marker = matches!(get_path(merged, field), Some(Value::Object(map))
            if map.len() == 1
                && map.get(SECRET_CLEAR_MARKER) == Some(&Value::Bool(true)));
        if is_clear_marker && set_path(merged, field, Value::String(String::new())) {
            forced_cleared.insert(field);
        }
    }
}

/// 候选 secret 字段为 null、空字符串或等于 [`SECRET_MASK`]（Web 页面回传的
/// 掩码）时，保留当前库中的值（不覆盖）；`forced_cleared` 中的路径已由
/// [`apply_secret_clear_markers`] 写入空串，直接跳过保留。
pub(super) fn retain_secret_fields(
    merged: &mut Map<String, Value>,
    current: &Map<String, Value>,
    forced_cleared: &BTreeSet<&'static str>,
) {
    for field in SECRET_PATHS {
        if forced_cleared.contains(field) {
            continue;
        }
        let Some(current_value) = get_path(current, field) else {
            continue;
        };
        let should_retain = match get_path(merged, field) {
            None => true,
            Some(value) => {
                value.is_null()
                    || value
                        .as_str()
                        .is_some_and(|text| text.is_empty() || text == SECRET_MASK)
            }
        };
        if should_retain {
            let _ = set_path(merged, field, current_value.clone());
        }
    }
}

/// 强制用当前库中的值覆盖候选 secret 字段（回滚时防止旧值/掩码覆盖当前密钥）。
pub(super) fn force_retain_secret_fields(
    merged: &mut Map<String, Value>,
    current: &Map<String, Value>,
) {
    for field in SECRET_PATHS {
        if let Some(current_value) = get_path(current, field) {
            let _ = set_path(merged, field, current_value.clone());
        }
    }
}

/// 递归比较两份 JSON，收集值不同的点路径（对象键逐层拼接，数组整体比较）。
pub(super) fn collect_changed_paths(
    current: &Value,
    candidate: &Value,
    prefix: &str,
    out: &mut Vec<String>,
) {
    match (current, candidate) {
        (Value::Object(current_map), Value::Object(candidate_map)) => {
            let keys = current_map
                .keys()
                .chain(candidate_map.keys())
                .collect::<BTreeSet<_>>();
            for key in keys {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                match (current_map.get(key), candidate_map.get(key)) {
                    (Some(before), Some(after)) => {
                        collect_changed_paths(before, after, &path, out);
                    }
                    _ => out.push(path),
                }
            }
        }
        _ => {
            if current != candidate {
                out.push(prefix.to_string());
            }
        }
    }
}

/// 递归遍历 JSON，命中 [`SECRET_PATHS`] 点路径的值替换为 [`SECRET_MASK`]
/// （保留字段存在性）；数组元素不追踪索引路径。
pub fn mask_secrets(value: &mut Value) {
    mask_secrets_inner(value, "");
}

pub(super) fn mask_secrets_inner(value: &mut Value, path: &str) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                if SECRET_PATHS.contains(&child_path.as_str()) {
                    *child = Value::String(SECRET_MASK.to_string());
                } else {
                    mask_secrets_inner(child, &child_path);
                }
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                mask_secrets_inner(item, path);
            }
        }
        _ => {}
    }
}
