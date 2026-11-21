//! 配置中心的对外结果类型：保存/回滚结果、字段级错误与历史版本信息。
//!
//! 这些类型是 `interfaces/http` 的业务契约：路由按**变体**分类，不解析错误文案，
//! 所以 [`ConfigSaveError`] 保持结构化三分——`Conflict` / `RevisionNotFound` 有专属
//! 响应，`Other` 里可能挂着可 downcast 的 [`ConfigFieldErrors`] 供表单逐字段高亮。
//!
//! 字段路径提取（[`field_errors_from_anyhow`]）是本模块唯一的启发式逻辑：校验错误
//! 的中文文案里带点路径（如 `queue.max_size 必须大于 0`），取路径只为**定位**，
//! 不参与判定；提取不到就留空串，前端按整段错误显示。
//!
//! 错误枚举实现 `From<rusqlite::Error>` / `From<serde_json::Error>`，让读写路径
//! 里满地的 `?` 直接冒泡到这里，不必到处 `.map_err`。

/// 持久化的命令回复模板。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredReply {
    pub key: String,
    pub text: String,
}

/// 一次配置保存的结果。
#[derive(Debug)]
pub struct ConfigSaveOutcome {
    /// 新版本号（当前 revision + 1）。
    pub revision: u64,
    /// 变更字段点路径（如 "queue.max_size"），用于页面显示。
    pub changed_fields: Vec<String>,
}

/// 字段级配置错误（保存校验失败时逐字段报告）。
#[derive(Clone, Debug)]
pub struct ConfigFieldError {
    /// 所属顶层段；从字段路径首段推导，推导不到时为 ""。
    pub section: String,
    /// 点路径（如 "queue.max_size"），提取不到时为 ""。
    pub field: String,
    /// 错误消息原文。
    pub message: String,
}

/// 字段级错误集合：save 校验失败时作为错误返回，可从 anyhow::Error 中 downcast。
#[derive(Debug)]
pub struct ConfigFieldErrors {
    pub errors: Vec<ConfigFieldError>,
}

impl std::fmt::Display for ConfigFieldErrors {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "配置校验失败，共 {} 个字段错误:",
            self.errors.len()
        )?;
        for error in &self.errors {
            write!(formatter, "\n- {}", error.message)?;
        }
        Ok(())
    }
}

impl std::error::Error for ConfigFieldErrors {}

/// 配置历史版本信息（按 revision 倒序返回）。
#[derive(Clone, Debug)]
pub struct ConfigRevisionInfo {
    pub revision: u64,
    pub created_at_ms: u64,
}

/// 一次 schema 平滑迁移的统计（只用于日志，不落库）。
#[derive(Clone, Copy, Debug, Default)]
struct MigrationOutcome {
    /// 迁移后写回的段数。
    rewritten_sections: usize,
    /// 相对于迁移前的 sections，值发生变化的字段点路径数。
    changed_fields: usize,
    /// 同步迁移的历史快照条数。
    migrated_revisions: usize,
}

/// 配置保存/回滚的结构化错误,路由层按变体分类,不依赖错误消息文本。
#[derive(Debug, thiserror::Error)]
pub enum ConfigSaveError {
    #[error("配置已被其他修改，请刷新后重试")]
    Conflict,
    #[error("目标版本 {0} 不存在")]
    RevisionNotFound(u64),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<rusqlite::Error> for ConfigSaveError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Other(error.into())
    }
}

impl From<serde_json::Error> for ConfigSaveError {
    fn from(error: serde_json::Error) -> Self {
        Self::Other(error.into())
    }
}
/// 从错误消息中提取字段点路径与所属 section；提取不到时 field/section 均为空串。
pub(super) fn extract_field_path(message: &str) -> (String, String) {
    if let Some(field) = extract_dotted_path(message) {
        let section = field.split('.').next().unwrap_or_default().to_string();
        return (field, section);
    }
    if let Some(field) = extract_backticked_field(message) {
        let section = if field.contains('.') {
            field.split('.').next().unwrap_or_default().to_string()
        } else {
            String::new()
        };
        return (field, section);
    }
    (String::new(), String::new())
}

/// 提取消息中第一个形如 [a-z_][a-z0-9_.]* 且含点的路径 token，
/// 用于 validate 的 bail 消息（如 "queue.max_size 必须大于 0"）。
pub(super) fn extract_dotted_path(message: &str) -> Option<String> {
    let bytes = message.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if !(bytes[index].is_ascii_lowercase() || bytes[index] == b'_') {
            index += 1;
            continue;
        }
        let start = index;
        index += 1;
        while index < bytes.len() {
            let byte = bytes[index];
            if byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'.' {
                index += 1;
            } else {
                break;
            }
        }
        let token = &message[start..index];
        if token.contains('.') {
            return Some(token.to_string());
        }
    }
    None
}

/// 提取 serde 反序列化错误消息中的字段名
/// （如 "missing field `max_size`"、"unknown field `foo`"）；
/// invalid type 等消息中反引号内容不是字段名，返回 None。
pub(super) fn extract_backticked_field(message: &str) -> Option<String> {
    for pattern in ["missing field `", "unknown field `", "duplicate field `"] {
        if let Some(start) = message.find(pattern) {
            let rest = &message[start + pattern.len()..];
            if let Some(end) = rest.find('`') {
                return Some(rest[..end].to_string());
            }
        }
    }
    None
}

/// 把单个错误消息转成字段级错误集合。
pub(super) fn field_errors_from_message(message: &str) -> ConfigFieldErrors {
    let (field, section) = extract_field_path(message);
    ConfigFieldErrors {
        errors: vec![ConfigFieldError {
            section,
            field,
            message: message.to_string(),
        }],
    }
}

/// 遍历 anyhow 错误链，取第一层能提取出字段路径的消息作为 field；
/// message 取该层原文（如 "queue.max_size 必须大于 0"）。
pub(super) fn field_errors_from_anyhow(error: &anyhow::Error) -> ConfigFieldErrors {
    for cause in error.chain() {
        let message = cause.to_string();
        let (field, section) = extract_field_path(&message);
        if !field.is_empty() {
            return ConfigFieldErrors {
                errors: vec![ConfigFieldError {
                    section,
                    field,
                    message,
                }],
            };
        }
    }
    ConfigFieldErrors {
        errors: vec![ConfigFieldError {
            section: String::new(),
            field: String::new(),
            message: error.to_string(),
        }],
    }
}
