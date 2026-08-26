//! SQLite 配置中心：ConfigStore（配置持久化存储）。
//!
//! 配置三表（config_meta / config_sections / config_revisions）与既有表
//! （request_state、cached_* 等）共存于统一数据库；配置 schema 版本
//! （[`CONFIG_SCHEMA_VERSION`]）与请求状态快照版本、缓存 user_version 完全独立。
//! schema 不兼容时明确失败，不做迁移。
//!
//! 启动流程读取完整配置，Web 配置中心负责保存、历史版本和回滚。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::OptionalExtension;
use serde_json::{Map, Value};

use super::schema::{SECRET_PATHS, config_sections};
use super::{AppConfig, BootstrapConfig};

/// 配置数据库 schema 版本；与请求状态快照版本 2、缓存 user_version 1 完全独立。
pub const CONFIG_SCHEMA_VERSION: i64 = 1;

/// 打开连接（含头初始化）的重试次数：两个进程并发首次创建同一新库文件时，
/// SQLite 头初始化阶段的锁竞争发生在 busy_timeout 生效之前，需要有限重试吸收。
const OPEN_CONNECTION_RETRIES: usize = 3;

/// 共享配置存储句柄（阶段 4 起由 HTTP 接口持有）。
pub type SharedConfigStore = Arc<Mutex<ConfigStore>>;

/// secret 字段脱敏展示时使用的掩码字符串（Web 页面显示与表单回传值）。
/// 保存时提交值等于该掩码视为「未修改」，保留库中当前值，防止覆盖真实密钥。
pub const SECRET_MASK: &str = "••••••";

/// secret 字段清除标记的键名：提交值等于 JSON 对象 `{"__clear__": true}` 时
/// 表示**清除**该字段（写入空字符串，绕过保留规则）；Web 页面「清除」按钮使用。
pub const SECRET_CLEAR_MARKER: &str = "__clear__";

/// 配置持久化存储：config_sections 按段保存，config_meta 记录当前版本，
/// config_revisions 保存每次提交的完整快照（支持回滚）。
#[derive(Debug)]
pub struct ConfigStore {
    connection: rusqlite::Connection,
    executable_root: PathBuf,
    bootstrap: BootstrapConfig,
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

impl ConfigStore {
    /// 打开（必要时创建）配置数据库。
    ///
    /// 库不存在时创建三表并以 [`AppConfig::default()`] 初始化完整配置
    /// （分 section 写入 config_sections、meta 记 revision=1、写入 revision 1 快照）；
    /// 库已存在但 schema 版本不兼容时明确失败，不做迁移。
    /// 库中已有 request_state / cached_* 等其他表不视为冲突。
    pub fn open(
        database_path: &Path,
        executable_root: &Path,
        bootstrap: BootstrapConfig,
    ) -> Result<Self> {
        if let Some(parent) = database_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建配置数据库目录失败: {}", parent.display()))?;
        }
        // 两个进程同时首次打开同一新库文件时，SQLite 头初始化阶段的锁竞争发生在
        // busy_timeout 生效之前（连接尚未建立，busy handler 不存在），可能立即报
        // database is locked；有限重试（间隔递增）吸收该竞争窗口。
        let mut connection = None;
        let mut last_error = None;
        for attempt in 0..OPEN_CONNECTION_RETRIES {
            match Self::open_connection(database_path) {
                Ok(opened) => {
                    connection = Some(opened);
                    break;
                }
                Err(error) if attempt + 1 < OPEN_CONNECTION_RETRIES => {
                    last_error = Some(error);
                    std::thread::sleep(Duration::from_millis(100 * (attempt as u64 + 1)));
                }
                Err(error) => {
                    last_error = Some(error);
                    break;
                }
            }
        }
        let mut connection = connection.ok_or_else(|| {
            last_error.unwrap_or_else(|| {
                anyhow::anyhow!("打开配置数据库失败: {}", database_path.display())
            })
        })?;
        Self::ensure_schema(&mut connection, database_path)?;
        Ok(Self {
            connection,
            executable_root: executable_root.to_path_buf(),
            bootstrap,
        })
    }

    /// 打开连接并设置连接参数（WAL 等）；独立成函数供 open 的并发重试使用。
    fn open_connection(database_path: &Path) -> Result<rusqlite::Connection> {
        let connection = rusqlite::Connection::open(database_path)
            .with_context(|| format!("打开配置数据库失败: {}", database_path.display()))?;
        Self::initialize_connection(&connection).with_context(|| {
            format!(
                "初始化配置数据库连接失败（文件可能不是 SQLite 数据库）: {}",
                database_path.display()
            )
        })?;
        Ok(connection)
    }

    /// 设置连接参数：WAL、synchronous=NORMAL、foreign_keys=ON、busy_timeout=5000。
    /// busy_timeout 必须先设置：journal_mode=WAL 切换需要独占锁，两个进程并发
    /// 首次打开同一新库时后到者要能等待前者的锁释放，否则立即报 database is locked。
    fn initialize_connection(connection: &rusqlite::Connection) -> Result<()> {
        connection.pragma_update(None, "busy_timeout", 5_000i64)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.pragma_update(None, "foreign_keys", true)?;
        Ok(())
    }

    /// 校验或创建配置三表（三态状态机）。
    ///
    /// 三表（config_meta / config_sections / config_revisions）全不存在时按内置
    /// 默认配置初始化；三表全存在时逐个校验列结构（列名/类型/NOT NULL）与
    /// config_meta 的 id 主键与 CHECK 约束，再检查 schema 版本与初始化行；
    /// 只存在部分表时明确失败（绝不初始化），避免把半初始化/损坏库当新库重建。
    fn ensure_schema(connection: &mut rusqlite::Connection, path: &Path) -> Result<()> {
        const CONFIG_TABLES: [&str; 3] = ["config_meta", "config_sections", "config_revisions"];
        // 期望列：(列名, SQLite 声明类型, 是否要求 NOT NULL)。
        // 主键列（INTEGER/TEXT PRIMARY KEY）在 PRAGMA table_info 中 notnull 为 0，
        // 按 SQLite 实际行为声明；其余列均声明 NOT NULL。
        const META_COLUMNS: [(&str, &str, bool); 4] = [
            ("id", "INTEGER", false),
            ("schema_version", "INTEGER", true),
            ("revision", "INTEGER", true),
            ("updated_at_ms", "INTEGER", true),
        ];
        const SECTIONS_COLUMNS: [(&str, &str, bool); 4] = [
            ("section", "TEXT", false),
            ("value_json", "TEXT", true),
            ("revision", "INTEGER", true),
            ("updated_at_ms", "INTEGER", true),
        ];
        const REVISIONS_COLUMNS: [(&str, &str, bool); 3] = [
            ("revision", "INTEGER", false),
            ("snapshot_json", "TEXT", true),
            ("created_at_ms", "INTEGER", true),
        ];
        let mut existing_count = 0;
        {
            let mut statement = connection
                .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?1")?;
            for table in CONFIG_TABLES {
                if statement.exists(rusqlite::params![table])? {
                    existing_count += 1;
                }
            }
        }
        match existing_count {
            // 三表全不存在：全新库，按内置默认配置初始化。
            // 初始化内部在 IMMEDIATE 事务内重查 config_meta，处理与另一进程
            // 并发首次打开同一新库的 TOCTOU 竞争（见 initialize_database）。
            0 => return Self::initialize_database(connection, path),
            // 部分表存在：半初始化或结构损坏，绝不初始化（否则会把已有数据当新库覆盖）。
            count if count != CONFIG_TABLES.len() => {
                bail!(
                    "配置数据库表结构不匹配（配置表缺失），请删除 {} 后重启",
                    path.display()
                );
            }
            _ => {}
        }
        // 三表全存在：逐个校验列结构（列名/声明类型/NOT NULL），
        // config_meta 额外校验 id 主键绑定与 CHECK (id = 1) 约束语义
        // （基于 SQLite 自身元数据与建表 SQL 解析，不做字符串包含猜测）；
        // 任一不满足都明确失败，避免运行期才暴露。
        if !table_structure_matches(connection, "config_meta", &META_COLUMNS)?
            || !config_meta_constraints_match(connection)?
            || !table_structure_matches(connection, "config_sections", &SECTIONS_COLUMNS)?
            || !table_structure_matches(connection, "config_revisions", &REVISIONS_COLUMNS)?
        {
            bail!("配置数据库表结构不匹配，请删除 {} 后重启", path.display());
        }
        // 三表全部通过检查后再读 schema 版本。
        let schema_version: Option<i64> = connection
            .query_row(
                "SELECT schema_version FROM config_meta WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let Some(schema_version) = schema_version else {
            bail!(
                "配置数据库 config_meta 缺少初始化数据，请删除 {} 后重启",
                path.display()
            );
        };
        if schema_version != CONFIG_SCHEMA_VERSION {
            bail!(
                "配置数据库 schema 版本不兼容（当前 {schema_version}，需要 {CONFIG_SCHEMA_VERSION}），请删除 {} 后重启",
                path.display()
            );
        }
        Ok(())
    }

    /// 创建配置三表，并以 [`AppConfig::default()`] 写入初始配置（revision 1）。
    ///
    /// 事务外检查「三表全不存在」与事务内建表之间存在 TOCTOU 窗口：两个进程
    /// 同时首次打开同一新库时，后取到写锁的一方在事务内可能发现 config_meta
    /// 已存在（另一方刚完成初始化并提交）。因此本函数在 IMMEDIATE 事务内
    /// 先重查 config_meta：已存在则回滚本次事务（不做任何写入），回到
    /// [`Self::ensure_schema`] 的三表全存在完整校验路径（结构/schema 版本校验），
    /// 而不是盲目 INSERT 造成主键冲突启动失败。
    fn initialize_database(connection: &mut rusqlite::Connection, path: &Path) -> Result<()> {
        // Immediate：立即取写锁，避免与并发初始化/保存的 deferred 事务互相升级造成 busy 死锁。
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // 事务内重查：另一进程可能已在本事务取到写锁前完成初始化并提交
        // （事务外检查时其未提交更改不可见，事务外判定「三表全不存在」可能已过期）。
        let already_initialized = transaction
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'config_meta'",
            )?
            .exists([])?;
        if already_initialized {
            // 另一进程刚完成初始化：回滚本次事务，重新走三表全存在的完整校验路径。
            drop(transaction);
            return Self::ensure_schema(connection, path);
        }
        Self::initialize_database_transaction(&transaction, now_ms() as i64)?;
        transaction.commit()?;
        Ok(())
    }

    /// 在已开启的 IMMEDIATE 事务内建三表并写入内置默认配置（revision 1）。
    /// 独立成函数供测试模拟「另一进程正在初始化」的持锁场景。
    fn initialize_database_transaction(
        transaction: &rusqlite::Transaction,
        timestamp: i64,
    ) -> Result<()> {
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS config_meta (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                schema_version INTEGER NOT NULL,
                revision INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS config_sections (
                section TEXT PRIMARY KEY,
                value_json TEXT NOT NULL,
                revision INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS config_revisions (
                revision INTEGER PRIMARY KEY,
                snapshot_json TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL
            );",
        )?;
        let default_value = AppConfig::default().to_db_value();
        let sections = default_value
            .as_object()
            .context("默认配置序列化结果必须是对象")?;
        for (section, section_value) in sections {
            transaction.execute(
                "INSERT INTO config_sections (section, value_json, revision, updated_at_ms)
                 VALUES (?1, ?2, 1, ?3)",
                rusqlite::params![section, serde_json::to_string(section_value)?, timestamp],
            )?;
        }
        transaction.execute(
            "INSERT INTO config_revisions (revision, snapshot_json, created_at_ms)
             VALUES (1, ?1, ?2)",
            rusqlite::params![serde_json::to_string(&default_value)?, timestamp],
        )?;
        transaction.execute(
            "INSERT INTO config_meta (id, schema_version, revision, updated_at_ms)
             VALUES (1, ?1, 1, ?2)",
            rusqlite::params![CONFIG_SCHEMA_VERSION, timestamp],
        )?;
        Ok(())
    }

    /// 加载完整业务配置：读全部 config_sections 组装 JSON，注入启动引导
    /// （http/logging/state.playback_state_path），解析相对路径并通过完整校验。
    pub fn load_full(&self) -> Result<AppConfig> {
        let mut sections = self.read_all_sections()?;
        // 旧版本库可能残留已删除字段（如 playback.kugou_api_*），先剔除
        // 再严格反序列化，避免 deny_unknown_fields 导致启动失败。
        prune_unknown_schema_fields(&mut sections);
        let config = AppConfig::from_db_value(
            Value::Object(sections),
            &self.bootstrap,
            &self.executable_root,
        )
        .context("从配置数据库组装完整配置失败")?;
        config.validate().context("数据库中的配置未通过校验")?;
        Ok(config)
    }

    /// 返回脱敏后的配置 JSON（供 Web 显示）：section 合并后的 JSON，
    /// secret 字段替换为 "••••••"（保留字段存在性）。
    ///
    /// 与 [`Self::load_full`] 不同：本方法**不解析相对路径**，返回库中存储的
    /// 原始值（如 `deps/assets/idioms.txt`），避免 Web 表单回填绝对路径后
    /// 保存把绝对路径写回配置库。http/logging 段不落库，这里按启动引导
    /// （config.yaml）原始值注入；state.playback_state_path 注入统一数据库路径。
    pub fn current_value(&self) -> Result<Value> {
        let mut value = Value::Object(self.read_all_sections()?);
        fill_missing_defaults(&mut value, &AppConfig::default().to_db_value());
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "http".to_string(),
                serde_json::to_value(&self.bootstrap.http)?,
            );
            object.insert(
                "logging".to_string(),
                serde_json::to_value(&self.bootstrap.logging)?,
            );
            if let Some(state) = object.get_mut("state").and_then(Value::as_object_mut) {
                state.insert(
                    "playback_state_path".to_string(),
                    serde_json::to_value(&self.bootstrap.database_path)?,
                );
            }
        }
        mask_secrets(&mut value);
        Ok(value)
    }

    /// 用候选 sections 组装完整配置（与保存同路径），返回校验错误列表；
    /// 校验通过时返回空列表。
    ///
    /// `sections` 为**整段替换**语义（与 [`ConfigStore::save`] 一致）：提交段
    /// 覆盖该段全部字段，未提交段保持原值；Web 表单必须按段提交完整值，
    /// 否则提交段中缺失的字段会被覆盖掉。
    pub fn validate_candidate(
        &self,
        sections: &Map<String, Value>,
    ) -> Result<Vec<ConfigFieldError>> {
        // 未提交段（current）同样先剔除旧版残留字段，保证保存路径与加载一致。
        let mut current_sections = self.read_all_sections()?;
        prune_unknown_schema_fields(&mut current_sections);
        let mut merged = merge_sections(&current_sections, sections);
        let immutable_errors = immutable_bootstrap_field_conflicts(&current_sections, &merged);
        if !immutable_errors.is_empty() {
            return Ok(immutable_errors);
        }
        // 与 save 同一路径：先应用清除标记，再保留其余 secret（保证预检结果
        // 与最终保存一致，清除标记不会被误判为非法类型）。
        let mut forced_cleared = BTreeSet::new();
        apply_secret_clear_markers(&mut merged, &mut forced_cleared);
        retain_secret_fields(&mut merged, &current_sections, &forced_cleared);
        let config = match self.build_config(&merged) {
            Ok(config) => config,
            Err(error) => return Ok(field_errors_from_message(&error.to_string()).errors),
        };
        match config.validate() {
            Ok(()) => Ok(Vec::new()),
            Err(error) => Ok(field_errors_from_anyhow(&error).errors),
        }
    }

    /// 保存配置：候选段整段替换（并集覆盖），完整校验通过后单事务写入。
    ///
    /// `sections` 为**整段替换**语义：提交段覆盖该段全部字段（提交段中缺失的
    /// 字段视为删除），未提交段保持原值；Web 表单必须按段提交完整值。
    ///
    /// secret 字段（ai.api_key 等）提交值为 null、空字符串或 [`SECRET_MASK`]
    /// （Web 页面回传的掩码）时保留当前值；提交值等于 JSON 对象
    /// `{"__clear__": true}`（[`SECRET_CLEAR_MARKER`]）时清除该字段（写入空串）；
    /// base_revision 与库中当前版本不一致时拒绝写入。
    pub fn save(
        &mut self,
        base_revision: u64,
        sections: Map<String, Value>,
    ) -> std::result::Result<ConfigSaveOutcome, ConfigSaveError> {
        // 应用候选：并集合并、剔除注入段、secret 空值/掩码保留、
        // `{"__clear__": true}` 标记强制清除（写入空串，绕过保留规则）。
        // current 先剔除旧版残留字段：加载时容忍、提交时清理，保存一次即迁移干净。
        let mut current_sections = self.read_all_sections()?;
        prune_unknown_schema_fields(&mut current_sections);
        let normalized_current = merge_sections(&current_sections, &Map::new());
        let mut merged = merge_sections(&current_sections, &sections);
        let immutable_errors = immutable_bootstrap_field_conflicts(&current_sections, &merged);
        if !immutable_errors.is_empty() {
            return Err(ConfigSaveError::Other(
                ConfigFieldErrors {
                    errors: immutable_errors,
                }
                .into(),
            ));
        }
        let mut forced_cleared = BTreeSet::new();
        apply_secret_clear_markers(&mut merged, &mut forced_cleared);
        retain_secret_fields(&mut merged, &current_sections, &forced_cleared);
        // 组装完整配置并校验；失败返回字段级错误，不写库。
        let config = match self.build_config(&merged) {
            Ok(config) => config,
            Err(error) => {
                return Err(ConfigSaveError::Other(
                    field_errors_from_message(&error.to_string()).into(),
                ));
            }
        };
        if let Err(error) = config.validate() {
            return Err(ConfigSaveError::Other(
                field_errors_from_anyhow(&error).into(),
            ));
        }
        // 单事务：写/更新 config_sections、插入完整快照（含 secret 明文，DB 本地）、
        // 更新 config_meta.revision；事务内重读 revision 与基线比对保证并发安全。
        // Immediate：立即取写锁，避免 HTTP 并发保存时 deferred 事务互相升级
        // 造成 SQLite busy 死锁（busy_timeout=5000 已有）。
        let timestamp = now_ms() as i64;
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let stored_revision: i64 =
            transaction.query_row("SELECT revision FROM config_meta WHERE id = 1", [], |row| {
                row.get(0)
            })?;
        if stored_revision as u64 != base_revision {
            return Err(ConfigSaveError::Conflict);
        }
        let next_revision = stored_revision as u64 + 1;
        for (section, section_value) in &merged {
            transaction.execute(
                "INSERT OR REPLACE INTO config_sections (section, value_json, revision, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    section,
                    serde_json::to_string(section_value)?,
                    next_revision as i64,
                    timestamp
                ],
            )?;
        }
        transaction.execute(
            "INSERT INTO config_revisions (revision, snapshot_json, created_at_ms)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![
                next_revision as i64,
                serde_json::to_string(&Value::Object(merged.clone()))?,
                timestamp
            ],
        )?;
        transaction.execute(
            "UPDATE config_meta SET revision = ?1, updated_at_ms = ?2 WHERE id = 1",
            rusqlite::params![next_revision as i64, timestamp],
        )?;
        // 只保留最近 50 版历史快照,防止配置库无限膨胀。
        transaction.execute(
            "DELETE FROM config_revisions WHERE revision <= ?1",
            rusqlite::params![next_revision as i64 - 50],
        )?;
        transaction.commit()?;
        // 计算变更字段点路径（候选 vs 当前）。
        let mut changed_fields = Vec::new();
        collect_changed_paths(
            &Value::Object(normalized_current),
            &Value::Object(merged),
            "",
            &mut changed_fields,
        );
        Ok(ConfigSaveOutcome {
            revision: next_revision,
            changed_fields,
        })
    }

    /// 按 revision 倒序返回全部历史版本。
    pub fn revisions(&self) -> Result<Vec<ConfigRevisionInfo>> {
        let mut statement = self.connection.prepare(
            "SELECT revision, created_at_ms FROM config_revisions ORDER BY revision DESC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(ConfigRevisionInfo {
                revision: row.get::<_, i64>(0)? as u64,
                created_at_ms: row.get::<_, i64>(1)? as u64,
            })
        })?;
        let mut infos = Vec::new();
        for row in rows {
            infos.push(row?);
        }
        Ok(infos)
    }

    /// 回滚到目标版本：读目标快照作为候选全段（secret 字段保留当前值，
    /// 防止历史快照中的掩码或旧值覆盖当前密钥），走 save 校验/事务，
    /// 记录新版本（当前 +1）。
    pub fn rollback(
        &mut self,
        target_revision: u64,
        base_revision: u64,
    ) -> std::result::Result<ConfigSaveOutcome, ConfigSaveError> {
        let snapshot: Option<String> = self
            .connection
            .query_row(
                "SELECT snapshot_json FROM config_revisions WHERE revision = ?1",
                rusqlite::params![target_revision as i64],
                |row| row.get(0),
            )
            .optional()?;
        let Some(snapshot) = snapshot else {
            return Err(ConfigSaveError::RevisionNotFound(target_revision));
        };
        let value: Value = serde_json::from_str(&snapshot).map_err(ConfigSaveError::from)?;
        let mut sections = value
            .as_object()
            .cloned()
            .ok_or_else(|| anyhow!("目标版本快照必须是对象"))?;
        // 旧版本历史快照同样可能携带已删除字段，先剔除再走 save 校验。
        prune_unknown_schema_fields(&mut sections);
        let current_sections = self.read_all_sections()?;
        force_retain_secret_fields(&mut sections, &current_sections);
        self.save(base_revision, sections)
    }

    /// 测试读取启动引导配置，确认脱敏不会修改运行态密钥。
    #[cfg(test)]
    pub fn bootstrap(&self) -> &BootstrapConfig {
        &self.bootstrap
    }

    /// 读取当前全部 config_sections。
    fn read_all_sections(&self) -> Result<Map<String, Value>> {
        let mut statement = self
            .connection
            .prepare("SELECT section, value_json FROM config_sections")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut sections = Map::new();
        for row in rows {
            let (section, text) = row?;
            let value: Value = serde_json::from_str(&text)
                .with_context(|| format!("解析配置段 {section} 失败"))?;
            sections.insert(section, value);
        }
        Ok(sections)
    }

    /// 读取 config_meta 的当前 revision。
    pub fn current_revision(&self) -> Result<u64> {
        let revision: i64 = self.connection.query_row(
            "SELECT revision FROM config_meta WHERE id = 1",
            [],
            |row| row.get(0),
        )?;
        Ok(revision as u64)
    }

    /// 由 sections 组装完整配置（注入启动引导并解析相对路径）。
    fn build_config(&self, sections: &Map<String, Value>) -> Result<AppConfig> {
        AppConfig::from_db_value(
            Value::Object(sections.clone()),
            &self.bootstrap,
            &self.executable_root,
        )
    }

    /// 仅测试：在 config_meta 表上注入 BEFORE UPDATE 触发器，
    /// 使后续任何保存写库失败并整体回滚（模拟写盘故障）。
    #[cfg(test)]
    pub(crate) fn inject_write_failure(&self) -> Result<()> {
        self.connection.execute_batch(
            "CREATE TRIGGER config_store_write_failure BEFORE UPDATE ON config_meta
             BEGIN SELECT RAISE(ABORT, '注入的写盘故障'); END",
        )?;
        Ok(())
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

    /// 从数据库存储值组装完整配置：http/logging/state 段缺失时走 serde 默认值，
    /// 随后注入启动引导提供的 http/logging 与统一数据库路径，并解析相对路径
    /// （state.playback_state_path 已是绝对路径，再次解析保持不变）。
    pub(crate) fn from_db_value(
        value: Value,
        bootstrap: &BootstrapConfig,
        executable_root: &Path,
    ) -> Result<AppConfig> {
        let mut config: AppConfig = serde_json::from_value(value)?;
        config.state.playback_state_path = bootstrap.database_path.clone();
        config.http = bootstrap.http.clone();
        config.logging = bootstrap.logging.clone();
        config.resolve_runtime_paths(executable_root);
        config.playback.normalize_audio_cache_paths(executable_root);
        Ok(config)
    }
}

/// 剔除 config_sections 中当前 schema 未声明的字段（如旧版本库遗留的
/// `playback.kugou_api_executable` / `playback.kugou_api_base_url`），
/// 保证严格反序列化（deny_unknown_fields）不被历史数据破坏。
///
/// 只用于**库中已有数据**：启动加载、保存时未提交段（current）与回滚快照。
/// 客户端提交段不经过此处，未知字段照旧被 save 拒绝（防篡改语义不变）。
fn prune_unknown_schema_fields(sections: &mut Map<String, Value>) {
    let known_by_section = config_sections()
        .into_iter()
        .map(|section| {
            let mut keys = BTreeSet::new();
            for field in &section.fields {
                if let Some(rest) = field.path.strip_prefix(&format!("{}.", section.name))
                    && let Some(leaf) = rest.split('.').next()
                {
                    keys.insert(leaf.to_string());
                }
            }
            (section.name, keys)
        })
        .collect::<BTreeMap<_, _>>();
    for (name, value) in sections.iter_mut() {
        let Some(known) = known_by_section.get(name) else {
            continue;
        };
        if let Some(object) = value.as_object_mut() {
            object.retain(|key, _| known.contains(key));
        }
    }
}

/// Existing databases can omit fields introduced with a serde default. Expose those fields to
/// configuration clients without changing explicit values (including null) or resolving paths.
fn fill_missing_defaults(current: &mut Value, defaults: &Value) {
    let (Some(current), Some(defaults)) = (current.as_object_mut(), defaults.as_object()) else {
        return;
    };
    for (key, default) in defaults {
        match current.get_mut(key) {
            Some(value) => fill_missing_defaults(value, default),
            None => {
                current.insert(key.clone(), default.clone());
            }
        }
    }
}

/// 当前毫秒时间戳（系统时钟不可用时回退 0）。
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// 检查表是否存在且列结构匹配期望列（列集合必须完全一致，顺序不限）；
/// 每列同时校验列名、SQLite 声明类型（declared type，比较忽略大小写）与
/// NOT NULL 标志（notnull=1）。表缺失或任一不匹配时返回 false。
///
/// `expected_columns` 元素为 (列名, 声明类型, 是否要求 NOT NULL)；主键列在
/// PRAGMA table_info 中 notnull 为 0，期望值应按 SQLite 实际行为声明。
fn table_structure_matches(
    connection: &rusqlite::Connection,
    table: &str,
    expected_columns: &[(&str, &str, bool)],
) -> Result<bool> {
    let exists: bool = connection
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?1")?
        .exists(rusqlite::params![table])?;
    if !exists {
        return Ok(false);
    }
    // 实际列：(列名, 声明类型, notnull 标志)；PRAGMA table_info 的 type 为
    // 建表时声明的原文（SQLite 不做大小写归一），比较时统一转小写。
    let mut columns = Vec::new();
    {
        let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)? != 0,
            ))
        })?;
        for row in rows {
            columns.push(row?);
        }
    }
    if columns.len() != expected_columns.len() {
        return Ok(false);
    }
    // 逐期望列匹配：列名一致、声明类型大小写不敏感一致、NOT NULL 标志一致。
    for (expected_name, expected_type, expected_not_null) in expected_columns {
        let Some((_, declared_type, not_null)) = columns
            .iter()
            .find(|(name, _, _)| name.as_str() == *expected_name)
        else {
            return Ok(false);
        };
        if !declared_type.eq_ignore_ascii_case(expected_type) || *not_null != *expected_not_null {
            return Ok(false);
        }
    }
    Ok(true)
}

/// 校验 config_meta 的约束（基于 SQLite 自身元数据，不做字符串包含猜测）：
///
/// 1. 主键绑定：优先用 `PRAGMA index_list` 找 `origin = 'pk'` 的索引，
///    再用 `PRAGMA index_info` 取其列，主键必须恰好是 `id` 一列；
///    `id INTEGER PRIMARY KEY` 单列是 rowid 别名，index_list 不返回行，
///    此时回退 `PRAGMA table_info` 的 `pk` 列序（pk > 0 表示属于主键，
///    INTEGER PRIMARY KEY 单列时 id.pk = 1 且其余列为 0）；
/// 2. CHECK 语义：从 `sqlite_master.sql` 建表原文解析 CHECK 表达式
///    （词法扫描提取，字符串/注释/引用标识符中的 check 字样忽略），
///    规范化（小写、去空白）后必须精确等于 `check(id=1)` —— 单一
///    `id = 1` 比较，不允许 OR/AND、其他列或其他常量放宽；
///    允许 `CHECK (id = 1)` / `CHECK(id=1)` 等空白差异，以及
///    `CONSTRAINT <名> CHECK (id = 1)` 前缀形式（约束名可引号包裹）。
///
/// 表缺失或任一约束不符合时返回 false。
fn config_meta_constraints_match(connection: &rusqlite::Connection) -> Result<bool> {
    // 1) 主键绑定：收集全部 origin='pk' 索引的列（非 rowid 别名主键
    //    才有 sqlite_autoindex 索引）。
    let mut pk_columns = Vec::new();
    let mut found_pk_index = false;
    {
        let mut statement = connection.prepare("PRAGMA index_list('config_meta')")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, String>(3)?))
        })?;
        for row in rows {
            let (index_name, origin) = row?;
            if origin != "pk" {
                continue;
            }
            found_pk_index = true;
            let mut info = connection.prepare(&format!("PRAGMA index_info({index_name})"))?;
            let columns = info.query_map([], |row| row.get::<_, String>(2))?;
            for column in columns {
                pk_columns.push(column?);
            }
        }
    }
    if found_pk_index {
        // 有主键索引：主键必须恰好是 id 一列。
        if pk_columns.len() != 1 || pk_columns[0] != "id" {
            return Ok(false);
        }
    } else {
        // 无主键索引：INTEGER PRIMARY KEY 单列（rowid 别名）。
        // table_info.pk 是列在（复合）主键中的序号（1 起），
        // 必须是 id 单独占主键（id.pk = 1 且其余列 pk = 0）。
        let mut pk_flagged_columns = Vec::new();
        {
            let mut statement = connection.prepare("PRAGMA table_info(config_meta)")?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, i64>(5)?))
            })?;
            for row in rows {
                let (name, pk) = row?;
                if pk > 0 {
                    pk_flagged_columns.push(name);
                }
            }
        }
        if pk_flagged_columns.len() != 1 || pk_flagged_columns[0] != "id" {
            return Ok(false);
        }
    }
    // 2) CHECK 语义：解析建表 SQL 中的 CHECK 表达式并精确比对。
    let sql: Option<String> = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'config_meta'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let Some(sql) = sql else {
        return Ok(false);
    };
    let checks = extract_check_constraints(&sql);
    if checks.is_empty() {
        return Ok(false);
    }
    Ok(checks
        .iter()
        .all(|check| normalize_check(check) == "check(id=1)"))
}

/// 从建表 SQL 中提取全部 CHECK 约束子句（含 check 关键字与括号，保留原文
/// 大小写与空白）。sqlite_master.sql 保存的是建表语句原文，格式确定，
/// 用最小 SQL 词法扫描器单遍扫描：单引号字符串常量（含 `''` 转义）、
/// `--` 行注释、`/* */` 块注释、双引号/方括号/反引号引用标识符整体跳过
/// （其中的 check 字样不视为关键字，避免伪匹配），其余位置找词边界的
/// `check` 关键字，括号配对取完整表达式（允许括号嵌套，如
/// `CHECK ((id = 1))`；字符串/注释/引用标识符内的括号不参与配对）。
fn extract_check_constraints(sql: &str) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut clauses = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        // 词法元素（字符串/注释/引用标识符）整体跳过，其中的 check 不视为关键字。
        let skipped = skip_lexical_element(bytes, index);
        if skipped != index {
            index = skipped;
            continue;
        }
        // 大小写不敏感的 check 关键字，要求前后都不是标识符字符（词边界）。
        let is_check = bytes[index].eq_ignore_ascii_case(&b'c')
            && bytes
                .get(index + 1)
                .is_some_and(|byte| byte.eq_ignore_ascii_case(&b'h'))
            && bytes
                .get(index + 2)
                .is_some_and(|byte| byte.eq_ignore_ascii_case(&b'e'))
            && bytes
                .get(index + 3)
                .is_some_and(|byte| byte.eq_ignore_ascii_case(&b'c'))
            && bytes
                .get(index + 4)
                .is_some_and(|byte| byte.eq_ignore_ascii_case(&b'k'))
            && bytes
                .get(index + 5)
                .is_none_or(|byte| !is_identifier_byte(*byte))
            && (index == 0 || !is_identifier_byte(bytes[index - 1]));
        if !is_check {
            index += 1;
            continue;
        }
        // 跳过关键字后的空白与注释，下一个非空白字符必须是 '('。
        let mut cursor = index + 5;
        loop {
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if cursor >= bytes.len() {
                break;
            }
            let skipped = skip_lexical_element(bytes, cursor);
            if skipped == cursor {
                break;
            }
            cursor = skipped;
        }
        if bytes.get(cursor) != Some(&b'(') {
            index += 1;
            continue;
        }
        // 括号配对：取到与左括号匹配的右括号为止（允许嵌套；
        // 字符串/注释/引用标识符内的括号不参与配对）。
        let mut depth = 0i32;
        let mut end = None;
        while cursor < bytes.len() {
            let skipped = skip_lexical_element(bytes, cursor);
            if skipped != cursor {
                cursor = skipped;
                continue;
            }
            match bytes[cursor] {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(cursor);
                        break;
                    }
                }
                _ => {}
            }
            cursor += 1;
        }
        if let Some(end) = end {
            clauses.push(sql[index..=end].to_string());
            index = end + 1;
        } else {
            index += 1;
        }
    }
    clauses
}

/// 标识符字节（check 关键字词边界与约束名剥离的字节版判断）。
/// 非 ASCII 字节（>= 0x80）一律视为标识符延续：UTF-8 多字节序列的每个字节
/// 都 >= 0x80，`check` 后紧跟非 ASCII 字符（如 `checké`）不是关键字边界。
fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$') || byte >= 0x80
}

/// 若 `bytes[i..]` 以 SQL 词法元素开头，返回跳过该元素后的下标：
/// - `'...'` 单引号字符串常量（`''` 转义，未闭合按到结尾）；
/// - `-- ...` 行注释（到换行或结尾）；
/// - `/* ... */` 块注释（未闭合按到结尾）；
/// - `"..."` 双引号、`[...]` 方括号、`` `...` `` 反引号引用标识符
///   （双引号内 `""` 转义，未闭合按到结尾）。
///   否则返回 `i` 原值。调用方保证 `i < bytes.len()`。
fn skip_lexical_element(bytes: &[u8], i: usize) -> usize {
    match bytes[i] {
        b'\'' => {
            // 单引号字符串：`''` 转义；未闭合按到结尾。
            let mut j = i + 1;
            while j < bytes.len() {
                if bytes[j] == b'\'' {
                    if bytes.get(j + 1) == Some(&b'\'') {
                        j += 2;
                    } else {
                        return j + 1;
                    }
                } else {
                    j += 1;
                }
            }
            bytes.len()
        }
        b'-' if bytes.get(i + 1) == Some(&b'-') => {
            // -- 行注释：到换行或结尾（停在换行处，由主循环继续处理）。
            let mut j = i + 2;
            while j < bytes.len() && bytes[j] != b'\n' {
                j += 1;
            }
            j
        }
        b'/' if bytes.get(i + 1) == Some(&b'*') => {
            // /* */ 块注释：未闭合按到结尾。
            let mut j = i + 2;
            while j + 1 < bytes.len() {
                if bytes[j] == b'*' && bytes[j + 1] == b'/' {
                    return j + 2;
                }
                j += 1;
            }
            bytes.len()
        }
        b'"' => {
            // 双引号引用标识符：`""` 转义；未闭合按到结尾。
            let mut j = i + 1;
            while j < bytes.len() {
                if bytes[j] == b'"' {
                    if bytes.get(j + 1) == Some(&b'"') {
                        j += 2;
                    } else {
                        return j + 1;
                    }
                } else {
                    j += 1;
                }
            }
            bytes.len()
        }
        b'[' => {
            // 方括号引用标识符：到 ] 结束；未闭合按到结尾。
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != b']' {
                j += 1;
            }
            if j < bytes.len() { j + 1 } else { bytes.len() }
        }
        b'`' => {
            // 反引号引用标识符：到 ` 结束；未闭合按到结尾。
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != b'`' {
                j += 1;
            }
            if j < bytes.len() { j + 1 } else { bytes.len() }
        }
        _ => i,
    }
}

/// 规范化 CHECK 子句并剥离可选的 `constraint <标识符>` 前缀：
/// 剥离在原文上进行（保留空白/换行，使 `--` 行注释能正确终止），
/// `CONSTRAINT` 与约束名之间、约束名与 `CHECK` 之间允许空白与 SQL 注释；
/// 约束名可为裸标识符或引号包裹形式（".." / [..] / `..`，引号内支持 `""` 转义），
/// 剥离后剩余部分小写化、去空白（如 `check(id=1)`）返回。
fn normalize_check(clause: &str) -> String {
    let bytes = clause.as_bytes();
    // 跳过前导空白与注释，定位可能的 constraint 关键字。
    let index = skip_whitespace_and_comments(bytes, 0);
    // 大小写不敏感的 constraint 关键字，要求前后都是词边界
    // （避免把 constrainta 之类标识符误当关键字）。
    let head = &bytes[index..];
    let matches_constraint = head.len() >= 10
        && head[..10].eq_ignore_ascii_case(b"constraint")
        && head.get(10).is_none_or(|byte| !is_identifier_byte(*byte))
        && (index == 0 || !is_identifier_byte(bytes[index - 1]));
    if !matches_constraint {
        return normalize_sql(clause);
    }
    // 跳过 CONSTRAINT 与约束名之间的空白/注释。
    let mut index = skip_whitespace_and_comments(bytes, index + 10);
    // 约束名：引号包裹（".." / [..] / `..`，含 "" 转义）或裸标识符。
    index = match bytes.get(index) {
        Some(b'"') | Some(b'[') | Some(b'`') => skip_lexical_element(bytes, index),
        Some(byte) if is_identifier_byte(*byte) => {
            let mut end = index + 1;
            while end < bytes.len() && is_identifier_byte(bytes[end]) {
                end += 1;
            }
            end
        }
        _ => index,
    };
    // 跳过约束名与 CHECK 之间的空白/注释，剩余部分即为 CHECK 表达式。
    let index = skip_whitespace_and_comments(bytes, index);
    normalize_sql(&clause[index..])
}

/// 从 `bytes[i..]` 起连续跳过空白与 SQL 注释（`--` 行注释、`/* */` 块注释），
/// 返回第一个既非空白也非注释的字节下标；`i` 可以等于 `bytes.len()`。
/// 只跳过注释，不跳过引号/字符串（约束名等需要单独剥离）。
fn skip_whitespace_and_comments(bytes: &[u8], mut i: usize) -> usize {
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            return i;
        }
        let is_comment = match bytes[i] {
            b'-' => bytes.get(i + 1) == Some(&b'-'),
            b'/' => bytes.get(i + 1) == Some(&b'*'),
            _ => false,
        };
        if !is_comment {
            return i;
        }
        i = skip_lexical_element(bytes, i);
    }
}

/// SQL 文本规范化：小写化并去掉全部空白（比较约束片段、CHECK 表达式时
/// 忽略排版差异）。
fn normalize_sql(sql: &str) -> String {
    sql.chars()
        .filter(|ch| !ch.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

/// 合并候选 sections：当前与提交的并集，提交段整体覆盖。
/// 同时剔除由启动引导注入的 http/logging 段与 state.playback_state_path
/// （这些项不在库中持久化，写入与展示时都不出现）。
fn merge_sections(
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
fn get_path<'a>(root: &'a Map<String, Value>, path: &str) -> Option<&'a Value> {
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
fn set_path(map: &mut Map<String, Value>, path: &str, value: Value) -> bool {
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

/// 应用 secret 清除标记：候选值等于 JSON 对象 `{"__clear__": true}`
/// （[`SECRET_CLEAR_MARKER`]）时，把 merged 中该路径写为空字符串，并记入
/// `forced_cleared` 集合（[`retain_secret_fields`] 跳过该路径，防止保留规则
/// 把清除请求还原成当前值）。必须在 `retain_secret_fields` 之前调用。
fn apply_secret_clear_markers(
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
fn retain_secret_fields(
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
fn force_retain_secret_fields(merged: &mut Map<String, Value>, current: &Map<String, Value>) {
    for field in SECRET_PATHS {
        if let Some(current_value) = get_path(current, field) {
            let _ = set_path(merged, field, current_value.clone());
        }
    }
}

/// 递归比较两份 JSON，收集值不同的点路径（对象键逐层拼接，数组整体比较）。
fn collect_changed_paths(current: &Value, candidate: &Value, prefix: &str, out: &mut Vec<String>) {
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

fn mask_secrets_inner(value: &mut Value, path: &str) {
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

/// 从错误消息中提取字段点路径与所属 section；提取不到时 field/section 均为空串。
fn extract_field_path(message: &str) -> (String, String) {
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
fn extract_dotted_path(message: &str) -> Option<String> {
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
fn extract_backticked_field(message: &str) -> Option<String> {
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
fn field_errors_from_message(message: &str) -> ConfigFieldErrors {
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
fn field_errors_from_anyhow(error: &anyhow::Error) -> ConfigFieldErrors {
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

/// 引导段字段：由 config.yaml 启动引导固化，Web 配置中心不能修改。
/// 防止经 HTTP 篡改 login_helper_exe 等路径后自动拉起任意可执行文件。
const IMMUTABLE_BOOTSTRAP_FIELDS: &[(&str, &str)] = &[
    ("playback", "loginHelperExecutable"),
    ("startup", "exePath"),
    ("startup", "gameArgs"),
];

/// 对比当前已存储的值：候选合并结果中受保护字段与当前一致（Web 表单整段
/// 回传原值）时放行，被改动时拒绝并给出字段错误。
fn immutable_bootstrap_field_conflicts(
    current: &Map<String, Value>,
    merged: &Map<String, Value>,
) -> Vec<ConfigFieldError> {
    let mut errors = Vec::new();
    for (section, field) in IMMUTABLE_BOOTSTRAP_FIELDS {
        let merged_value = merged
            .get(*section)
            .and_then(Value::as_object)
            .and_then(|object| object.get(*field));
        let current_value = current
            .get(*section)
            .and_then(Value::as_object)
            .and_then(|object| object.get(*field));
        if current_value != merged_value {
            errors.push(ConfigFieldError {
                section: (*section).to_string(),
                field: format!("{section}.{field}"),
                message: format!(
                    "{}.{} 由 config.yaml 启动固化，Web 面板不可修改；请直接编辑 config.yaml 后重启",
                    section, field
                ),
            });
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Barrier;

    use super::*;
    use crate::config::{HttpConfig, LoggingConfig};
    use serde_json::json;

    /// 构造测试用最小启动配置；统一数据库路径指向临时目录下的 playback.sqlite3
    /// （StateConfig::validate 要求 playback_state_path 文件名必须是 playback.sqlite3）。
    fn test_bootstrap(database_path: &Path) -> BootstrapConfig {
        BootstrapConfig {
            database_path: database_path.to_path_buf(),
            http: HttpConfig::default(),
            logging: LoggingConfig::default(),
        }
    }

    /// 创建临时根目录与数据库路径；deps/data 目录预先创建
    /// （coexist 测试在 open 之前手工建表需要父目录存在）。
    fn temp_database(name: &str) -> (PathBuf, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("config-store-{name}-{}", uuid::Uuid::new_v4()));
        let database_path = root.join("deps/data/playback.sqlite3");
        fs::create_dir_all(database_path.parent().unwrap()).unwrap();
        (root, database_path)
    }

    /// 构造「内置默认 + 启动引导注入 + 路径解析」后的期望配置，
    /// 与 ConfigStore::load_full 的处理路径一致。
    fn expected_loaded_config(bootstrap: &BootstrapConfig, executable_root: &Path) -> AppConfig {
        let mut expected = AppConfig::default();
        expected.state.playback_state_path = bootstrap.database_path.clone();
        expected.http = bootstrap.http.clone();
        expected.logging = bootstrap.logging.clone();
        expected.resolve_runtime_paths(executable_root);
        expected
            .playback
            .normalize_audio_cache_paths(executable_root);
        expected
    }

    fn cleanup(root: &Path) {
        let _ = fs::remove_dir_all(root);
    }

    /// 读取 config_meta 的当前 revision（独立连接）。
    fn stored_revision(database_path: &Path) -> i64 {
        let connection = rusqlite::Connection::open(database_path).unwrap();
        connection
            .query_row("SELECT revision FROM config_meta WHERE id = 1", [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    /// 读取 config_sections 中某段的 JSON 值（独立连接）。
    fn stored_section(database_path: &Path, section: &str) -> Value {
        let connection = rusqlite::Connection::open(database_path).unwrap();
        let text: String = connection
            .query_row(
                "SELECT value_json FROM config_sections WHERE section = ?1",
                rusqlite::params![section],
                |row| row.get(0),
            )
            .unwrap();
        serde_json::from_str(&text).unwrap()
    }

    /// 读取 config_revisions 最新快照的 JSON 值（独立连接）。
    fn latest_snapshot(database_path: &Path) -> Value {
        let connection = rusqlite::Connection::open(database_path).unwrap();
        let text: String = connection
            .query_row(
                "SELECT snapshot_json FROM config_revisions ORDER BY revision DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        serde_json::from_str(&text).unwrap()
    }

    /// 与 initialize_database 一致的真实建表语句（用于手工构造完整/损坏的配置库）。
    const CORRECT_META_SQL: &str = "CREATE TABLE config_meta (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        schema_version INTEGER NOT NULL,
        revision INTEGER NOT NULL,
        updated_at_ms INTEGER NOT NULL
    )";
    const CORRECT_SECTIONS_SQL: &str = "CREATE TABLE config_sections (
        section TEXT PRIMARY KEY,
        value_json TEXT NOT NULL,
        revision INTEGER NOT NULL,
        updated_at_ms INTEGER NOT NULL
    )";
    const CORRECT_REVISIONS_SQL: &str = "CREATE TABLE config_revisions (
        revision INTEGER PRIMARY KEY,
        snapshot_json TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL
    )";

    /// 用真实 CREATE TABLE 语句手工创建配置三表并写入 config_meta 初始化行；
    /// 各表 SQL 可定制（如改列类型、去掉约束）以模拟结构损坏。
    fn create_config_tables(
        connection: &rusqlite::Connection,
        meta_sql: &str,
        sections_sql: &str,
        revisions_sql: &str,
    ) {
        connection
            .execute_batch(&format!("{meta_sql}; {sections_sql}; {revisions_sql};"))
            .unwrap();
        connection
            .execute(
                "INSERT INTO config_meta (id, schema_version, revision, updated_at_ms)
                 VALUES (1, 1, 1, 0)",
                [],
            )
            .unwrap();
    }

    /// 补插内置默认配置段（与 initialize_database 的写入一致），
    /// 用于手工建库后验证 open 可用。
    fn insert_default_sections(connection: &rusqlite::Connection) {
        let default_value = AppConfig::default().to_db_value();
        for (section, section_value) in default_value.as_object().unwrap() {
            connection
                .execute(
                    "INSERT INTO config_sections (section, value_json, revision, updated_at_ms)
                     VALUES (?1, ?2, 1, 0)",
                    rusqlite::params![section, serde_json::to_string(section_value).unwrap()],
                )
                .unwrap();
        }
    }

    /// 以当前完整配置为基底构造可提交的 sections 映射。
    fn full_sections(store: &ConfigStore) -> Map<String, Value> {
        store
            .load_full()
            .unwrap()
            .to_db_value()
            .as_object()
            .cloned()
            .unwrap()
    }

    #[test]
    fn legacy_unknown_section_fields_are_pruned_before_strict_deserialization() {
        let (root, database_path) = temp_database("legacy-prune");
        let bootstrap = test_bootstrap(&database_path);
        let store = ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap();

        // 把 playback 段改写为旧版本库形态：携带已删除的 kugou_api_* 字段。
        let playback = store.load_full().unwrap().playback.clone();
        let mut playback_value = serde_json::to_value(playback.clone()).unwrap();
        playback_value["kugou_api_executable"] = json!("kugou-api.exe");
        playback_value["kugou_api_base_url"] = json!("http://127.0.0.1:3000");
        let connection = rusqlite::Connection::open(&database_path).unwrap();
        connection
            .execute(
                "INSERT OR REPLACE INTO config_sections (section, value_json, revision, updated_at_ms)
                 VALUES ('playback', ?1, 1, 0)",
                rusqlite::params![serde_json::to_string(&playback_value).unwrap()],
            )
            .unwrap();
        drop(connection);

        let mut store = ConfigStore::open(&database_path, &root, bootstrap).unwrap();
        let loaded = store.load_full().unwrap();
        assert_eq!(
            loaded.playback.credential_directory, playback.credential_directory,
            "旧字段剔除不得影响已知字段"
        );
        assert_eq!(
            loaded.playback.login_timeout_ms, playback.login_timeout_ms,
            "旧字段剔除不得影响已知字段"
        );
        // 保存任意段：current 旧字段先剔除 → save 成功，库中被清理。
        let mut save_sections = Map::new();
        let current = store.current_value().unwrap();
        save_sections.insert("window".to_string(), current["window"].clone());
        store.save(1, save_sections).unwrap();
        let stored = stored_section(&database_path, "playback");
        assert!(
            stored.get("kugou_api_executable").is_none()
                && stored.get("kugou_api_base_url").is_none(),
            "保存后库中旧字段必须被清理: {stored}"
        );
        cleanup(&root);
    }

    #[test]
    fn legacy_timing_without_lyrics_lead_uses_and_exposes_the_default() {
        let (root, database_path) = temp_database("legacy-lyrics-lead");
        let bootstrap = test_bootstrap(&database_path);
        let store = ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap();
        drop(store);

        let mut timing = stored_section(&database_path, "timing");
        timing["playback"]
            .as_object_mut()
            .expect("playback timing object")
            .remove("lyrics_lead_seconds");
        let connection = rusqlite::Connection::open(&database_path).unwrap();
        connection
            .execute(
                "UPDATE config_sections SET value_json = ?1 WHERE section = 'timing'",
                rusqlite::params![serde_json::to_string(&timing).unwrap()],
            )
            .unwrap();
        drop(connection);

        let reopened = ConfigStore::open(&database_path, &root, bootstrap).unwrap();
        assert_eq!(
            reopened
                .load_full()
                .unwrap()
                .timing
                .playback
                .lyrics_lead_seconds,
            0.0
        );
        assert_eq!(
            reopened.current_value().unwrap()["timing"]["playback"]["lyrics_lead_seconds"].as_f64(),
            Some(0.0)
        );
        drop(reopened);
        cleanup(&root);
    }

    #[test]
    fn prune_keeps_nested_object_fields_declared_in_schema() {
        let mut sections = Map::new();
        sections.insert(
            "playback".to_string(),
            json!({
                "credential_directory": "deps/data/credentials",
                "audio_cache": {
                    "enabled": true,
                    "directory": ""
                },
                "stale_legacy_field": "remove-me"
            }),
        );
        prune_unknown_schema_fields(&mut sections);
        let playback = sections["playback"].as_object().unwrap();
        assert!(playback.contains_key("credential_directory"));
        assert!(playback.contains_key("audio_cache"));
        assert!(!playback.contains_key("stale_legacy_field"));
    }

    #[test]
    fn new_database_initializes_with_default_config() {
        let (root, database_path) = temp_database("init-default");
        let bootstrap = test_bootstrap(&database_path);
        let store = ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap();

        let loaded = store.load_full().unwrap();
        let expected = expected_loaded_config(&bootstrap, &root);
        assert_eq!(
            serde_json::to_value(&loaded).unwrap(),
            serde_json::to_value(&expected).unwrap(),
            "新库初始化后配置必须与内置默认值（注入启动引导并解析路径后）一致"
        );
        assert_eq!(
            loaded.state.playback_state_path, database_path,
            "state.playback_state_path 必须注入统一数据库路径"
        );
        assert_eq!(
            loaded.http.port, bootstrap.http.port,
            "http 段必须由启动引导提供"
        );
        assert_eq!(
            loaded.logging.dir,
            root.join("deps/logs"),
            "logging 段必须由启动引导提供并解析为绝对路径"
        );
        assert_eq!(
            store
                .validate_candidate(&full_sections(&store))
                .unwrap()
                .len(),
            0,
            "默认配置候选必须通过校验"
        );
        assert_eq!(
            stored_revision(&database_path),
            1,
            "初始化后 revision 必须为 1"
        );
        let revisions = store.revisions().unwrap();
        assert_eq!(revisions.len(), 1, "初始化后必须只有一条历史快照");
        assert_eq!(revisions[0].revision, 1, "初始化快照必须是 revision 1");

        // 共享句柄必须可正常加锁访问。
        let shared: SharedConfigStore = Arc::new(Mutex::new(store));
        assert_eq!(shared.lock().unwrap().current_revision().unwrap(), 1);
        cleanup(&root);
    }

    #[test]
    fn full_config_round_trip_survives_reopen() {
        let (root, database_path) = temp_database("round-trip");
        let bootstrap = test_bootstrap(&database_path);
        let mut store = ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap();

        let mut sections = full_sections(&store);
        sections["queue"]["max_size"] = json!(20);
        sections["timing"]["loop_idle_ms"] = json!(100);
        let outcome = store.save(1, sections.clone()).unwrap();
        assert_eq!(outcome.revision, 2, "首次保存后版本必须为 2");
        assert!(
            outcome
                .changed_fields
                .contains(&"queue.max_size".to_string()),
            "changed_fields 必须包含 queue.max_size: {:?}",
            outcome.changed_fields
        );
        assert!(
            outcome
                .changed_fields
                .contains(&"timing.loop_idle_ms".to_string()),
            "changed_fields 必须包含 timing.loop_idle_ms: {:?}",
            outcome.changed_fields
        );
        drop(store);

        let reopened = ConfigStore::open(&database_path, &root, bootstrap).unwrap();
        let loaded = reopened.load_full().unwrap();
        assert_eq!(loaded.queue.max_size, 20, "重开后 queue.max_size 必须保留");
        assert_eq!(
            loaded.timing.loop_idle_ms, 100,
            "重开后 timing.loop_idle_ms 必须保留"
        );
        cleanup(&root);
    }

    #[test]
    fn unknown_section_or_field_is_rejected() {
        let (root, database_path) = temp_database("unknown-field");
        let bootstrap = test_bootstrap(&database_path);
        let mut store = ConfigStore::open(&database_path, &root, bootstrap).unwrap();

        // 未知顶层段：反序列化必须失败并报告段名。
        let mut sections = full_sections(&store);
        sections.insert("bogus_section".to_string(), json!({"a": 1}));
        let error = store.save(1, sections).unwrap_err();
        assert!(
            error.to_string().contains("bogus_section"),
            "未知段必须被拒绝并报告段名: {error}"
        );
        let ConfigSaveError::Other(error) = error else {
            panic!("save 的字段错误必须包装在 Other 中");
        };
        let field_errors = error
            .downcast_ref::<ConfigFieldErrors>()
            .expect("save 的字段错误必须可 downcast 为 ConfigFieldErrors");
        assert_eq!(
            field_errors.errors[0].field, "bogus_section",
            "字段点路径必须提取到未知段名"
        );
        assert_eq!(
            field_errors.errors[0].section, "",
            "无点路径的字段无法推导 section，必须为空串"
        );

        // 未知字段：反序列化必须失败并报告字段名。
        let mut sections = full_sections(&store);
        sections["window"]
            .as_object_mut()
            .unwrap()
            .insert("bogus_field".to_string(), json!(1));
        let error = store.save(1, sections).unwrap_err();
        assert!(
            error.to_string().contains("bogus_field"),
            "未知字段必须被拒绝并报告字段名: {error}"
        );
        let ConfigSaveError::Other(error) = error else {
            panic!("save 的字段错误必须包装在 Other 中");
        };
        let field_errors = error
            .downcast_ref::<ConfigFieldErrors>()
            .expect("save 的字段错误必须可 downcast 为 ConfigFieldErrors");
        assert_eq!(
            field_errors.errors[0].field, "bogus_field",
            "字段点路径必须提取到未知字段名"
        );
        assert_eq!(
            field_errors.errors[0].section, "",
            "无点路径的字段无法推导 section，必须为空串"
        );
        assert_eq!(
            stored_revision(&database_path),
            1,
            "拒绝写入时 revision 必须不变"
        );
        cleanup(&root);
    }

    #[test]
    fn invalid_secret_parent_returns_validation_error_without_panicking() {
        let (root, database_path) = temp_database("invalid-secret-parent");
        let bootstrap = test_bootstrap(&database_path);
        let store = ConfigStore::open(&database_path, &root, bootstrap).unwrap();

        let mut sections = full_sections(&store);
        sections["song_review"]["provider"] = Value::Null;
        let errors = store.validate_candidate(&sections).unwrap();
        assert!(
            !errors.is_empty(),
            "secret 中间节点为 null 时必须返回校验错误，不能 panic"
        );
        assert_eq!(stored_revision(&database_path), 1, "校验失败不得写入新版本");
        cleanup(&root);
    }

    #[test]
    fn invalid_values_fail_validation_without_writing() {
        let (root, database_path) = temp_database("invalid-values");
        let bootstrap = test_bootstrap(&database_path);
        let mut store = ConfigStore::open(&database_path, &root, bootstrap).unwrap();

        // 窗口尺寸为 0：validate 拒绝。
        let mut sections = full_sections(&store);
        sections["window"]["content_width"] = json!(0);
        let error = store.save(1, sections).unwrap_err();
        assert!(
            error.to_string().contains("window.content_width"),
            "零窗口尺寸必须报告 window.content_width: {error}"
        );

        // 区域越界：validate 拒绝。
        let mut sections = full_sections(&store);
        sections["screen"]["chat_rect"]["x"] = json!(2000);
        let error = store.save(1, sections).unwrap_err();
        assert!(
            error.to_string().contains("screen.chat_rect"),
            "越界区域必须报告 screen.chat_rect: {error}"
        );

        assert_eq!(stored_revision(&database_path), 1, "校验失败不得写库");
        let queue = stored_section(&database_path, "queue");
        assert_eq!(queue["max_size"], json!(5), "校验失败不得改动已存段");
        cleanup(&root);
    }

    #[test]
    fn window_screen_cross_validation() {
        let (root, database_path) = temp_database("cross-validation");
        let bootstrap = test_bootstrap(&database_path);
        let mut store = ConfigStore::open(&database_path, &root, bootstrap).unwrap();

        // 只改 window 不改 screen：尺寸不一致必须被拒绝。
        let mut sections = full_sections(&store);
        sections["window"]["content_width"] = json!(1600);
        let error = store.save(1, sections).unwrap_err();
        assert!(
            error.to_string().contains("window.content_width"),
            "window 与 screen 不一致必须被拒绝: {error}"
        );
        assert_eq!(stored_revision(&database_path), 1, "交叉校验失败不得写库");
        cleanup(&root);
    }

    #[test]
    fn revision_conflict_is_rejected() {
        let (root, database_path) = temp_database("revision-conflict");
        let bootstrap = test_bootstrap(&database_path);
        let mut store = ConfigStore::open(&database_path, &root, bootstrap).unwrap();

        let sections = full_sections(&store);
        let error = store.save(0, sections).unwrap_err();
        assert!(
            error.to_string().contains("配置已被其他修改"),
            "过期基线必须报告冲突: {error}"
        );
        assert_eq!(stored_revision(&database_path), 1, "冲突时不得写库");
        cleanup(&root);
    }

    #[test]
    fn rollback_restores_a_previous_revision() {
        let (root, database_path) = temp_database("rollback");
        let bootstrap = test_bootstrap(&database_path);
        let mut store = ConfigStore::open(&database_path, &root, bootstrap).unwrap();

        // 首次保存：queue.max_size = 20 → revision 2。
        let mut first = full_sections(&store);
        first["queue"]["max_size"] = json!(20);
        store.save(1, first).unwrap();

        // 再次保存：queue.max_size = 30 → revision 3。
        let mut second = full_sections(&store);
        second["queue"]["max_size"] = json!(30);
        store.save(2, second).unwrap();

        // 回滚到第一次保存产生的版本（revision 2），revision 递增为 4。
        let outcome = store.rollback(2, 3).unwrap();
        assert_eq!(outcome.revision, 4, "回滚后必须记录新版本");
        let loaded = store.load_full().unwrap();
        assert_eq!(loaded.queue.max_size, 20, "回滚后必须恢复首次保存的值");
        let revisions = store.revisions().unwrap();
        assert_eq!(
            revisions
                .iter()
                .map(|info| info.revision)
                .collect::<Vec<_>>(),
            [4, 3, 2, 1],
            "历史版本必须按 revision 倒序"
        );
        assert!(
            revisions.iter().all(|info| info.created_at_ms > 0),
            "历史版本必须记录创建时间戳"
        );
        cleanup(&root);
    }

    #[test]
    fn unsupported_schema_version_fails_loudly() {
        let (root, database_path) = temp_database("schema-version");
        let bootstrap = test_bootstrap(&database_path);
        let store = ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap();
        drop(store);

        // 手工把 schema 版本改成 99，模拟未来版本写入的数据库。
        let connection = rusqlite::Connection::open(&database_path).unwrap();
        connection
            .execute(
                "UPDATE config_meta SET schema_version = 99 WHERE id = 1",
                [],
            )
            .unwrap();
        drop(connection);

        let error = ConfigStore::open(&database_path, &root, bootstrap)
            .expect_err("不兼容的 schema 版本必须明确失败");
        assert!(
            error.to_string().contains("不兼容"),
            "错误消息必须提示 schema 版本不兼容: {error}"
        );
        assert!(
            error.to_string().contains("请删除"),
            "错误消息必须提示删除数据库: {error}"
        );
        cleanup(&root);
    }

    #[test]
    fn secret_values_are_masked_in_current_value() {
        let (root, database_path) = temp_database("secret-mask");
        let bootstrap = test_bootstrap(&database_path);
        let mut store = ConfigStore::open(&database_path, &root, bootstrap).unwrap();

        let mut sections = full_sections(&store);
        sections["ai"]["api_key"] = json!("sk-test-123");
        store.save(1, sections).unwrap();

        let current = store.current_value().unwrap();
        assert_eq!(
            current["ai"]["api_key"], "••••••",
            "current_value 中 ai.api_key 必须脱敏"
        );
        assert!(
            current["ai"].get("api_key").is_some(),
            "脱敏必须保留字段存在性"
        );
        assert_eq!(
            store.load_full().unwrap().ai.api_key,
            "sk-test-123",
            "load_full 必须返回真实密钥"
        );
        cleanup(&root);
    }

    #[test]
    fn empty_secret_submission_keeps_previous_value() {
        let (root, database_path) = temp_database("secret-retention");
        let bootstrap = test_bootstrap(&database_path);
        let mut store = ConfigStore::open(&database_path, &root, bootstrap).unwrap();

        // 先保存真实密钥 → revision 2。
        let mut sections = full_sections(&store);
        sections["ai"]["api_key"] = json!("sk-test-456");
        store.save(1, sections).unwrap();

        // 用空字符串重新提交 ai 段 → 密钥必须保留 → revision 3。
        let mut sections = full_sections(&store);
        sections["ai"]["api_key"] = json!("");
        let outcome = store.save(2, sections).unwrap();
        assert_eq!(outcome.revision, 3, "空密钥提交必须正常保存");
        assert_eq!(
            store.load_full().unwrap().ai.api_key,
            "sk-test-456",
            "空密钥提交不得覆盖已保存的密钥"
        );
        cleanup(&root);
    }

    #[test]
    fn masked_secret_submission_keeps_previous_value() {
        let (root, database_path) = temp_database("masked-secret-retention");
        let bootstrap = test_bootstrap(&database_path);
        let mut store = ConfigStore::open(&database_path, &root, bootstrap).unwrap();

        // 先保存真实密钥 → revision 2。
        let mut sections = full_sections(&store);
        sections["ai"]["api_key"] = json!("sk-test-789");
        store.save(1, sections).unwrap();

        // 用掩码重新提交 ai 段（Web 表单原样回传掩码）→ 密钥必须保留 → revision 3。
        let mut sections = full_sections(&store);
        sections["ai"]["api_key"] = json!(SECRET_MASK);
        let outcome = store.save(2, sections).unwrap();
        assert_eq!(outcome.revision, 3, "掩码密钥提交必须正常保存");
        assert_eq!(
            store.load_full().unwrap().ai.api_key,
            "sk-test-789",
            "掩码密钥提交不得覆盖已保存的密钥"
        );
        // config_sections 与最新快照中必须仍是旧明文（数据库只存明文）。
        assert_eq!(
            stored_section(&database_path, "ai")["api_key"],
            json!("sk-test-789"),
            "config_sections 中 ai 段必须保留旧明文"
        );
        assert_eq!(
            latest_snapshot(&database_path)["ai"]["api_key"],
            json!("sk-test-789"),
            "config_revisions 最新快照必须保留旧明文"
        );
        assert!(
            !outcome.changed_fields.contains(&"ai.api_key".to_string()),
            "掩码提交未改变 secret 时 changed_fields 不得包含 ai.api_key: {:?}",
            outcome.changed_fields
        );
        cleanup(&root);
    }

    #[test]
    fn clear_marker_secret_removes_the_value() {
        let (root, database_path) = temp_database("secret-clear");
        let bootstrap = test_bootstrap(&database_path);
        let mut store = ConfigStore::open(&database_path, &root, bootstrap).unwrap();

        // 先保存真实密钥 → revision 2。
        let mut sections = full_sections(&store);
        sections["ai"]["api_key"] = json!("sk-clear-me");
        store.save(1, sections).unwrap();

        // 真实密钥存在时先走预检路径（validate_candidate）提交清除请求：
        // 覆盖“真实密钥存在时预检清除请求”的关键路径，
        // 且不得把 {"__clear__": true} 误判为非法类型。
        let mut sections = full_sections(&store);
        sections["ai"]["api_key"] = json!({ "__clear__": true });
        let errors = store.validate_candidate(&sections).unwrap();
        assert!(errors.is_empty(), "清除标记预检必须通过: {:?}", errors);

        // 预检通过后再提交 {"__clear__": true} → 密钥必须被清除（写入空串）→ revision 3。
        let mut sections = full_sections(&store);
        sections["ai"]["api_key"] = json!({ "__clear__": true });
        let outcome = store.save(2, sections).unwrap();
        assert_eq!(outcome.revision, 3, "清除标记必须正常保存并递增版本");
        assert_eq!(
            store.load_full().unwrap().ai.api_key,
            "",
            "清除标记必须把 api_key 写为空串"
        );
        assert!(
            outcome.changed_fields.contains(&"ai.api_key".to_string()),
            "清除 secret 必须记录为变更字段: {:?}",
            outcome.changed_fields
        );
        assert_eq!(
            stored_section(&database_path, "ai")["api_key"],
            json!(""),
            "config_sections 中 ai 段 api_key 必须为空串"
        );
        assert_eq!(
            latest_snapshot(&database_path)["ai"]["api_key"],
            json!(""),
            "最新快照中 ai 段 api_key 必须为空串"
        );
        cleanup(&root);
    }

    #[test]
    fn missing_config_sections_table_fails_loudly() {
        let (root, database_path) = temp_database("missing-sections-table");
        let bootstrap = test_bootstrap(&database_path);
        let store = ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap();
        drop(store);

        // 手工删掉 config_sections 表，模拟数据库结构损坏。
        let connection = rusqlite::Connection::open(&database_path).unwrap();
        connection
            .execute_batch("DROP TABLE config_sections")
            .unwrap();
        drop(connection);

        let error = ConfigStore::open(&database_path, &root, bootstrap)
            .expect_err("缺少 config_sections 表必须明确失败");
        assert!(
            error.to_string().contains("表结构不匹配"),
            "错误消息必须提示表结构不匹配: {error}"
        );
        assert!(
            error.to_string().contains("请删除"),
            "错误消息必须提示删除数据库: {error}"
        );
        cleanup(&root);
    }

    #[test]
    fn partial_config_tables_fail_loudly_without_initializing() {
        let (root, database_path) = temp_database("partial-tables");
        let bootstrap = test_bootstrap(&database_path);

        // 只建 config_meta（真实 CREATE TABLE 语句），模拟半初始化/结构损坏的库。
        let connection = rusqlite::Connection::open(&database_path).unwrap();
        connection.execute_batch(CORRECT_META_SQL).unwrap();
        connection
            .execute(
                "INSERT INTO config_meta (id, schema_version, revision, updated_at_ms)
                 VALUES (1, 1, 1, 0)",
                [],
            )
            .unwrap();
        drop(connection);

        let error = ConfigStore::open(&database_path, &root, bootstrap)
            .expect_err("部分配置表存在时必须明确失败，绝不初始化");
        assert!(
            error.to_string().contains("表结构不匹配"),
            "错误消息必须提示表结构不匹配: {error}"
        );
        assert!(
            error.to_string().contains("请删除"),
            "错误消息必须提示删除数据库: {error}"
        );

        // 绝不初始化：失败后其余配置表不得被创建。
        let connection = rusqlite::Connection::open(&database_path).unwrap();
        let sections_exists: bool = connection
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'config_sections'",
            )
            .unwrap()
            .exists([])
            .unwrap();
        assert!(
            !sections_exists,
            "部分表存在时不得初始化其余配置表（必须保持原状）"
        );
        cleanup(&root);
    }

    #[test]
    fn mismatched_column_type_fails_structure_check() {
        let (root, database_path) = temp_database("wrong-column-type");
        let bootstrap = test_bootstrap(&database_path);
        let connection = rusqlite::Connection::open(&database_path).unwrap();
        // 三表齐全但 config_sections.value_json 声明类型错误：TEXT → INTEGER。
        create_config_tables(
            &connection,
            CORRECT_META_SQL,
            "CREATE TABLE config_sections (
                section TEXT PRIMARY KEY,
                value_json INTEGER NOT NULL,
                revision INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL
            )",
            CORRECT_REVISIONS_SQL,
        );
        drop(connection);

        let error = ConfigStore::open(&database_path, &root, bootstrap)
            .expect_err("列声明类型不匹配必须明确失败");
        assert!(
            error.to_string().contains("表结构不匹配"),
            "错误消息必须提示表结构不匹配: {error}"
        );
        assert!(
            error.to_string().contains("请删除"),
            "错误消息必须提示删除数据库: {error}"
        );
        cleanup(&root);
    }

    #[test]
    fn missing_not_null_constraint_fails_structure_check() {
        let (root, database_path) = temp_database("missing-not-null");
        let bootstrap = test_bootstrap(&database_path);
        let connection = rusqlite::Connection::open(&database_path).unwrap();
        // 三表齐全但 config_sections.value_json 缺少 NOT NULL 约束。
        create_config_tables(
            &connection,
            CORRECT_META_SQL,
            "CREATE TABLE config_sections (
                section TEXT PRIMARY KEY,
                value_json TEXT,
                revision INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL
            )",
            CORRECT_REVISIONS_SQL,
        );
        drop(connection);

        let error = ConfigStore::open(&database_path, &root, bootstrap)
            .expect_err("缺少 NOT NULL 约束必须明确失败");
        assert!(
            error.to_string().contains("表结构不匹配"),
            "错误消息必须提示表结构不匹配: {error}"
        );
        assert!(
            error.to_string().contains("请删除"),
            "错误消息必须提示删除数据库: {error}"
        );
        cleanup(&root);
    }

    #[test]
    fn missing_primary_key_and_check_on_config_meta_fails() {
        let (root, database_path) = temp_database("meta-without-pk");
        let bootstrap = test_bootstrap(&database_path);
        let connection = rusqlite::Connection::open(&database_path).unwrap();
        // config_meta 的 id 缺少 PRIMARY KEY 与 CHECK (id = 1) 约束。
        create_config_tables(
            &connection,
            "CREATE TABLE config_meta (
                id INTEGER NOT NULL,
                schema_version INTEGER NOT NULL,
                revision INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL
            )",
            CORRECT_SECTIONS_SQL,
            CORRECT_REVISIONS_SQL,
        );
        drop(connection);

        let error = ConfigStore::open(&database_path, &root, bootstrap)
            .expect_err("config_meta 缺少主键与 CHECK 约束必须明确失败");
        assert!(
            error.to_string().contains("表结构不匹配"),
            "错误消息必须提示表结构不匹配: {error}"
        );
        assert!(
            error.to_string().contains("请删除"),
            "错误消息必须提示删除数据库: {error}"
        );
        cleanup(&root);
    }

    #[test]
    fn primary_key_bound_to_wrong_column_fails() {
        let (root, database_path) = temp_database("meta-pk-wrong-column");
        let bootstrap = test_bootstrap(&database_path);
        let connection = rusqlite::Connection::open(&database_path).unwrap();
        // 三表齐全、列结构与期望一致，但主键绑定到 schema_version 而非 id
        // （PRAGMA index_info 列集合必须是 id 一列，否则失败）。
        create_config_tables(
            &connection,
            "CREATE TABLE config_meta (
                id INTEGER,
                schema_version INTEGER NOT NULL,
                revision INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL,
                PRIMARY KEY (schema_version)
            )",
            CORRECT_SECTIONS_SQL,
            CORRECT_REVISIONS_SQL,
        );
        drop(connection);

        let error = ConfigStore::open(&database_path, &root, bootstrap)
            .expect_err("config_meta 主键绑定到错误列必须明确失败");
        assert!(
            error.to_string().contains("表结构不匹配"),
            "错误消息必须提示表结构不匹配: {error}"
        );
        assert!(
            error.to_string().contains("请删除"),
            "错误消息必须提示删除数据库: {error}"
        );
        cleanup(&root);
    }

    #[test]
    fn relaxed_check_constraint_fails() {
        // 三表齐全、主键绑定正确，但 CHECK 被放宽（IN 列表 / OR 放宽）：
        // 规范化后必须精确等于 check(id=1)，任何放宽都必须失败。
        for meta_sql in [
            "CREATE TABLE config_meta (
                id INTEGER PRIMARY KEY CHECK (id IN (1, 2)),
                schema_version INTEGER NOT NULL,
                revision INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL
            )",
            "CREATE TABLE config_meta (
                id INTEGER PRIMARY KEY CHECK (id = 1 OR id = 2),
                schema_version INTEGER NOT NULL,
                revision INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL
            )",
        ] {
            let (root, database_path) = temp_database("meta-relaxed-check");
            let bootstrap = test_bootstrap(&database_path);
            let connection = rusqlite::Connection::open(&database_path).unwrap();
            create_config_tables(
                &connection,
                meta_sql,
                CORRECT_SECTIONS_SQL,
                CORRECT_REVISIONS_SQL,
            );
            drop(connection);

            let error = ConfigStore::open(&database_path, &root, bootstrap)
                .expect_err("config_meta 的 CHECK 约束放宽必须明确失败");
            assert!(
                error.to_string().contains("表结构不匹配"),
                "错误消息必须提示表结构不匹配: {error}"
            );
            assert!(
                error.to_string().contains("请删除"),
                "错误消息必须提示删除数据库: {error}"
            );
            cleanup(&root);
        }
    }

    #[test]
    fn constraint_prefixed_check_on_config_meta_opens_successfully() {
        // CHECK 以 `CONSTRAINT <名> CHECK (id = 1)` 形式声明（表级约束）：
        // 语义校验必须先剥离 constraint 前缀再比对，open 必须成功。
        let (root, database_path) = temp_database("meta-constraint-prefix");
        let bootstrap = test_bootstrap(&database_path);
        let connection = rusqlite::Connection::open(&database_path).unwrap();
        create_config_tables(
            &connection,
            "CREATE TABLE config_meta (
                id INTEGER PRIMARY KEY,
                schema_version INTEGER NOT NULL,
                revision INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL,
                CONSTRAINT meta_single_row CHECK (id = 1)
            )",
            CORRECT_SECTIONS_SQL,
            CORRECT_REVISIONS_SQL,
        );
        // 补插默认配置段（与 initialize_database 的写入一致），证明 open 后可用。
        let default_value = AppConfig::default().to_db_value();
        for (section, section_value) in default_value.as_object().unwrap() {
            connection
                .execute(
                    "INSERT INTO config_sections (section, value_json, revision, updated_at_ms)
                     VALUES (?1, ?2, 1, 0)",
                    rusqlite::params![section, serde_json::to_string(section_value).unwrap()],
                )
                .unwrap();
        }
        drop(connection);

        let store = ConfigStore::open(&database_path, &root, bootstrap).unwrap();
        store.load_full().unwrap().validate().unwrap();
        cleanup(&root);
    }

    #[test]
    fn check_keyword_inside_string_literal_is_not_a_constraint() {
        // CHECK(id=1) 字样只出现在字符串字面量/行注释/块注释里，没有真实 CHECK：
        // 词法扫描必须跳过字符串与注释，提取不到 CHECK → open 必须失败。
        for meta_sql in [
            // 列默认值里的单引号字符串常量。
            "CREATE TABLE config_meta (
                id INTEGER PRIMARY KEY,
                schema_version INTEGER NOT NULL DEFAULT 'CHECK(id=1)',
                revision INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL
            )",
            // 块注释。
            "CREATE TABLE config_meta (
                id INTEGER PRIMARY KEY,
                schema_version INTEGER NOT NULL,
                revision INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL
                /* CHECK(id=1) */
            )",
            // 行注释。
            "CREATE TABLE config_meta (
                id INTEGER PRIMARY KEY,
                schema_version INTEGER NOT NULL,
                revision INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL
                -- CHECK(id=1)
            )",
        ] {
            let (root, database_path) = temp_database("meta-string-check");
            let bootstrap = test_bootstrap(&database_path);
            let connection = rusqlite::Connection::open(&database_path).unwrap();
            create_config_tables(
                &connection,
                meta_sql,
                CORRECT_SECTIONS_SQL,
                CORRECT_REVISIONS_SQL,
            );
            drop(connection);

            let error = ConfigStore::open(&database_path, &root, bootstrap)
                .expect_err("字符串/注释中的 CHECK 字样不得被当作真实约束");
            assert!(
                error.to_string().contains("表结构不匹配"),
                "错误消息必须提示表结构不匹配: {error}"
            );
            cleanup(&root);
        }
    }

    #[test]
    fn quoted_constraint_name_opens_successfully() {
        // 约束名用引号包裹（CONSTRAINT \"meta_single_row\" CHECK ...）：
        // 前缀剥离必须能处理引号包裹的约束名，open 必须成功。
        let (root, database_path) = temp_database("meta-quoted-constraint");
        let bootstrap = test_bootstrap(&database_path);
        let connection = rusqlite::Connection::open(&database_path).unwrap();
        create_config_tables(
            &connection,
            "CREATE TABLE config_meta (
                id INTEGER PRIMARY KEY,
                schema_version INTEGER NOT NULL,
                revision INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL,
                CONSTRAINT \"meta_single_row\" CHECK (id = 1)
            )",
            CORRECT_SECTIONS_SQL,
            CORRECT_REVISIONS_SQL,
        );
        insert_default_sections(&connection);
        drop(connection);

        let store = ConfigStore::open(&database_path, &root, bootstrap).unwrap();
        store.load_full().unwrap().validate().unwrap();
        cleanup(&root);
    }

    #[test]
    fn non_ascii_identifier_does_not_fake_check_keyword() {
        // `checké` 以 check 开头但 é 是标识符延续（UTF-8 多字节，非 ASCII）：
        // 词法扫描不得把它误判为 CHECK 关键字。无真实 CHECK → open 必须失败。
        let meta_sql = "CREATE TABLE config_meta (
            id INTEGER PRIMARY KEY,
            schema_version INTEGER NOT NULL,
            revision INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL,
            checké INTEGER NOT NULL DEFAULT 0
        )";
        assert!(
            extract_check_constraints(meta_sql).is_empty(),
            "check 后紧跟非 ASCII 标识符字符（checké）不得被识别为 CHECK"
        );
        let (root, database_path) = temp_database("meta-non-ascii-identifier");
        let bootstrap = test_bootstrap(&database_path);
        let connection = rusqlite::Connection::open(&database_path).unwrap();
        create_config_tables(
            &connection,
            meta_sql,
            CORRECT_SECTIONS_SQL,
            CORRECT_REVISIONS_SQL,
        );
        drop(connection);

        let error = ConfigStore::open(&database_path, &root, bootstrap)
            .expect_err("无真实 CHECK 约束的 config_meta 必须明确失败");
        assert!(
            error.to_string().contains("表结构不匹配"),
            "错误消息必须提示表结构不匹配: {error}"
        );
        cleanup(&root);
    }

    #[test]
    fn comments_inside_constraint_prefix_are_handled() {
        // CONSTRAINT 与约束名之间、约束名与 CHECK 之间出现 SQL 注释
        // （块注释/行注释）时，前缀剥离必须正确跳过，open 必须成功。
        for meta_sql in [
            // 块注释。
            "CREATE TABLE config_meta (
                id INTEGER PRIMARY KEY,
                schema_version INTEGER NOT NULL,
                revision INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL,
                CONSTRAINT /* c */ \"meta_single_row\" /* x */ CHECK (id = 1)
            )",
            // 行注释。
            "CREATE TABLE config_meta (
                id INTEGER PRIMARY KEY,
                schema_version INTEGER NOT NULL,
                revision INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL,
                CONSTRAINT -- c
                \"meta_single_row\" -- x
                CHECK (id = 1)
            )",
        ] {
            let (root, database_path) = temp_database("meta-constraint-comments");
            let bootstrap = test_bootstrap(&database_path);
            let connection = rusqlite::Connection::open(&database_path).unwrap();
            create_config_tables(
                &connection,
                meta_sql,
                CORRECT_SECTIONS_SQL,
                CORRECT_REVISIONS_SQL,
            );
            insert_default_sections(&connection);
            drop(connection);

            let store = ConfigStore::open(&database_path, &root, bootstrap).unwrap();
            store.load_full().unwrap().validate().unwrap();
            cleanup(&root);
        }
    }

    #[test]
    fn normalize_check_directly_strips_prefixed_constraint_names() {
        // 前缀剥离分支的直接单元覆盖（不经 open 全链路）：CONSTRAINT 与约束名
        // 之间夹块注释、约束名含 `""` 转义、约束名与 CHECK 之间夹行注释，
        // 剥离后必须精确等于 check(id=1)。
        assert_eq!(
            normalize_check("CONSTRAINT /* c */ \"a\"\"b\" -- x\n CHECK (id = 1)"),
            "check(id=1)"
        );
        // 无 constraint 前缀时直接规范化（去空白、小写）。
        assert_eq!(normalize_check("  CHECK (id = 1)  "), "check(id=1)");
        // 前缀不完整（constrainta）不是 constraint 关键字，原样规范化。
        assert_eq!(
            normalize_check("CONSTRAINTa CHECK (id = 1)"),
            "constraintacheck(id=1)"
        );
    }

    #[test]
    fn check_keyword_with_dollar_identifier_boundary_is_not_a_constraint() {
        // `check$...`：$ 是标识符字符（is_identifier_byte 包含 $），check 后紧跟
        // $ 不是关键字边界，不得提取为 CHECK；同段中真实的 CHECK(id=1) 仍要提取。
        let sql = "CREATE TABLE t (check$x INTEGER) CHECK(id=1)";
        assert_eq!(
            extract_check_constraints(sql),
            vec!["CHECK(id=1)"],
            "check$ 不得被当作 CHECK 关键字，真实 CHECK(id=1) 必须提取"
        );
    }

    #[test]
    fn escaped_quote_in_constraint_name_opens_successfully() {
        // 引号包裹的约束名内含 `""` 转义（CONSTRAINT "a""b" CHECK ...）：
        // 前缀剥离必须把整体当作一个约束名，open 必须成功。
        let (root, database_path) = temp_database("meta-escaped-quote-constraint");
        let bootstrap = test_bootstrap(&database_path);
        let connection = rusqlite::Connection::open(&database_path).unwrap();
        create_config_tables(
            &connection,
            "CREATE TABLE config_meta (
                id INTEGER PRIMARY KEY,
                schema_version INTEGER NOT NULL,
                revision INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL,
                CONSTRAINT \"a\"\"b\" CHECK (id = 1)
            )",
            CORRECT_SECTIONS_SQL,
            CORRECT_REVISIONS_SQL,
        );
        insert_default_sections(&connection);
        drop(connection);

        let store = ConfigStore::open(&database_path, &root, bootstrap).unwrap();
        store.load_full().unwrap().validate().unwrap();
        cleanup(&root);
    }

    #[test]
    fn composite_primary_key_containing_id_fails() {
        // 复合主键 (id, schema_version) 即使包含 id 也必须被拒绝：
        // config_meta 的主键必须恰好是 id 单列（rowid 别名或唯一主键索引）。
        let (root, database_path) = temp_database("meta-composite-pk");
        let bootstrap = test_bootstrap(&database_path);
        let connection = rusqlite::Connection::open(&database_path).unwrap();
        create_config_tables(
            &connection,
            "CREATE TABLE config_meta (
                id INTEGER,
                schema_version INTEGER NOT NULL,
                revision INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL,
                PRIMARY KEY (id, schema_version)
            )",
            CORRECT_SECTIONS_SQL,
            CORRECT_REVISIONS_SQL,
        );
        drop(connection);

        let error = ConfigStore::open(&database_path, &root, bootstrap)
            .expect_err("复合主键即使包含 id 也必须明确失败");
        assert!(
            error.to_string().contains("表结构不匹配"),
            "错误消息必须提示表结构不匹配: {error}"
        );
        assert!(
            error.to_string().contains("请删除"),
            "错误消息必须提示删除数据库: {error}"
        );
        cleanup(&root);
    }

    #[test]
    fn handcrafted_complete_config_schema_opens_successfully() {
        // 三表用与 initialize_database 一致的真实 CREATE TABLE 语句手工创建，
        // 模拟外部创建/迁移来的完整配置库：open 必须通过结构校验并可用。
        let (root, database_path) = temp_database("handcrafted-schema");
        let bootstrap = test_bootstrap(&database_path);
        let connection = rusqlite::Connection::open(&database_path).unwrap();
        create_config_tables(
            &connection,
            CORRECT_META_SQL,
            CORRECT_SECTIONS_SQL,
            CORRECT_REVISIONS_SQL,
        );
        // 补插默认配置段（模拟带数据的完整库，与 initialize_database 的写入一致）。
        let default_value = AppConfig::default().to_db_value();
        for (section, section_value) in default_value.as_object().unwrap() {
            connection
                .execute(
                    "INSERT INTO config_sections (section, value_json, revision, updated_at_ms)
                     VALUES (?1, ?2, 1, 0)",
                    rusqlite::params![section, serde_json::to_string(section_value).unwrap()],
                )
                .unwrap();
        }
        drop(connection);

        let mut store = ConfigStore::open(&database_path, &root, bootstrap).unwrap();
        store.load_full().unwrap().validate().unwrap();
        assert_eq!(
            stored_revision(&database_path),
            1,
            "手工建库的 revision 必须保持 1"
        );
        // 保存也正常（revision 2），证明三表全存在路径完整可用。
        let mut sections = full_sections(&store);
        sections["queue"]["max_size"] = json!(20);
        let outcome = store.save(1, sections).unwrap();
        assert_eq!(outcome.revision, 2, "手工建库必须可正常保存");
        cleanup(&root);
    }

    #[test]
    fn two_connections_same_base_revision_second_conflicts_then_retry_succeeds() {
        let (root, database_path) = temp_database("two-connections");
        let bootstrap = test_bootstrap(&database_path);
        let mut first = ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap();
        // 第二个独立连接打开同一数据库文件（模拟另一实例/进程持有句柄）。
        let mut second = ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap();

        // 两个实例都从 revision 1 出发：第一个保存成功 → revision 2。
        let mut sections = full_sections(&first);
        sections["queue"]["max_size"] = json!(20);
        first.save(1, sections).unwrap();
        assert_eq!(
            stored_revision(&database_path),
            2,
            "第一个实例保存后版本必须为 2"
        );

        // 第二个实例仍用过期基线 base=1 保存：事务内重读版本，必须冲突。
        let sections = full_sections(&second);
        let error = second.save(1, sections).unwrap_err();
        assert!(
            error.to_string().contains("配置已被其他修改"),
            "第二个实例保存过期基线必须报告冲突: {error}"
        );
        assert_eq!(stored_revision(&database_path), 2, "冲突时不得写库");

        // 失败方读取新版本后带新变更重试成功 → revision 3。
        let mut sections = full_sections(&second);
        sections["timing"]["loop_idle_ms"] = json!(100);
        let outcome = second.save(2, sections).unwrap();
        assert_eq!(outcome.revision, 3, "重试后必须保存成功");
        assert_eq!(stored_revision(&database_path), 3, "重试后版本必须为 3");
        assert_eq!(
            second.load_full().unwrap().timing.loop_idle_ms,
            100,
            "第二个实例重试保存的值必须生效"
        );
        cleanup(&root);
    }

    #[test]
    fn concurrent_initializer_commit_revalidates_instead_of_inserting() {
        // 两个进程同时首次打开同一新库（TOCTOU 场景）：连接 A 持 IMMEDIATE 写锁
        // 完成初始化但未提交，连接 B 的事务外检查看不到任何配置表（未提交更改
        // 不可见），进入初始化事务后必须事务内重查 config_meta——发现已初始化则
        // 回滚并走三表全存在的完整校验路径，而不是盲目 INSERT 造成主键冲突。
        let (root, database_path) = temp_database("concurrent-init");
        // 手工创建空库文件（无任何表）。
        let probe = rusqlite::Connection::open(&database_path).unwrap();
        drop(probe);

        // 连接 A：另一进程的初始化事务——IMMEDIATE 事务内建三表并写入全部默认
        // 数据，保持未提交以持有写锁（模拟初始化进行中）。
        let mut connection_a = rusqlite::Connection::open(&database_path).unwrap();
        let transaction_a = connection_a
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        ConfigStore::initialize_database_transaction(&transaction_a, 0).unwrap();

        // 连接 B：与 ConfigStore::open 相同的 busy_timeout；事务外检查时 A 未提交
        // → 三表不可见 → 进入 IMMEDIATE 初始化事务 → 阻塞等待 A 的写锁。
        let mut connection_b = rusqlite::Connection::open(&database_path).unwrap();
        connection_b
            .pragma_update(None, "busy_timeout", 5_000i64)
            .unwrap();
        let path = database_path.clone();
        let handle =
            std::thread::spawn(move || ConfigStore::ensure_schema(&mut connection_b, &path));
        // 等待 B 进入等待（被 A 的写锁阻塞）后提交 A 的事务释放写锁。
        std::thread::sleep(std::time::Duration::from_millis(300));
        transaction_a.commit().unwrap();

        handle
            .join()
            .expect("打开线程必须正常结束")
            .expect("并发初始化后打开必须成功（事务内重查回滚，而非主键冲突）");

        // 库中数据完整：初始化只发生一次，revision 1，默认配置可加载。
        assert_eq!(
            stored_revision(&database_path),
            1,
            "并发初始化后 revision 必须保持 1"
        );
        let store =
            ConfigStore::open(&database_path, &root, test_bootstrap(&database_path)).unwrap();
        store.load_full().unwrap().validate().unwrap();
        cleanup(&root);
    }

    #[test]
    fn concurrent_first_open_of_same_database_both_succeed() {
        // 两个进程同时首次打开同一新数据库文件：两个 open 都必须成功且 load_full
        // 正常（后取到写锁的一方经事务内重查走完整校验路径，不出现主键冲突）。
        let (root, database_path) = temp_database("concurrent-open");
        let barrier = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let barrier = Arc::clone(&barrier);
            let path = database_path.clone();
            let root = root.clone();
            let bootstrap = test_bootstrap(&database_path);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                ConfigStore::open(&path, &root, bootstrap).expect("并发首次打开必须成功")
            }));
        }
        barrier.wait();
        for handle in handles {
            let store = handle.join().expect("打开线程必须正常结束");
            store.load_full().unwrap().validate().unwrap();
        }
        assert_eq!(
            stored_revision(&database_path),
            1,
            "并发首次打开后初始化必须只发生一次"
        );
        cleanup(&root);
    }

    #[test]
    fn config_tables_coexist_with_request_state_and_cache_tables() {
        let (root, database_path) = temp_database("coexist");
        let bootstrap = test_bootstrap(&database_path);

        // 先建 request_state（与 features/playback/state.rs 结构一致）与 cached_tracks 表。
        let connection = rusqlite::Connection::open(&database_path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE request_state (
                    id INTEGER PRIMARY KEY CHECK (id = 1),
                    schema_version INTEGER NOT NULL,
                    snapshot TEXT NOT NULL
                );
                CREATE TABLE cached_tracks (hash TEXT PRIMARY KEY);",
            )
            .unwrap();
        drop(connection);

        // 配置表与既有表共存：open 成功、load_full 正常、校验通过。
        let store = ConfigStore::open(&database_path, &root, bootstrap).unwrap();
        store.load_full().unwrap().validate().unwrap();
        assert_eq!(
            stored_revision(&database_path),
            1,
            "共存库初始化后 revision 必须为 1"
        );
        cleanup(&root);
    }

    #[test]
    fn save_failure_keeps_previous_state() {
        let (root, database_path) = temp_database("save-failure");
        let bootstrap = test_bootstrap(&database_path);
        let mut store = ConfigStore::open(&database_path, &root, bootstrap).unwrap();

        // 注入 BEFORE UPDATE 触发器使 config_meta 更新失败（模拟写盘故障）。
        store.inject_write_failure().unwrap();

        let mut sections = full_sections(&store);
        sections["queue"]["max_size"] = json!(99);
        let error = store.save(1, sections).unwrap_err();
        assert!(
            error.to_string().contains("注入的写盘故障"),
            "必须报告注入的写盘故障: {error}"
        );

        assert_eq!(
            stored_revision(&database_path),
            1,
            "写失败后 revision 必须不变"
        );
        let queue = stored_section(&database_path, "queue");
        assert_eq!(
            queue["max_size"],
            json!(5),
            "写失败后 sections 必须保持原状"
        );
        cleanup(&root);
    }

    #[test]
    fn bootstrap_to_store_full_pipeline_initializes_a_valid_database() {
        // 集成验证（与主程序启动链路一致）：临时目录放精简 config.yaml，
        // 走 BootstrapConfig::load → ConfigStore::open → load_full 完整链路，
        // 新库必须自动以 AppConfig::default() 初始化并通过完整校验，
        // 引导注入（http/logging/state.playback_state_path）必须生效。
        let root =
            std::env::temp_dir().join(format!("config-store-pipeline-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let config_path = root.join("config.yaml");
        fs::write(
            &config_path,
            r#"
database_path: deps/data/playback.sqlite3
http:
  enabled: true
  host: 127.0.0.1
  port: 18888
  access_token: ""
logging:
  dir: deps/logs
  level: info
  rotate_daily: true
  retain_days: 7
"#,
        )
        .unwrap();

        let bootstrap = BootstrapConfig::load(&config_path, &root).expect("加载启动配置");
        let store = ConfigStore::open(&bootstrap.database_path, &root, bootstrap.clone())
            .expect("打开统一配置数据库");
        let config = store.load_full().expect("加载完整配置");
        config
            .validate()
            .expect("启动链路加载的配置必须通过完整校验");
        assert_eq!(
            config.state.playback_state_path, bootstrap.database_path,
            "state.playback_state_path 必须注入统一数据库路径"
        );
        assert_eq!(
            config.http.port, bootstrap.http.port,
            "http 段必须由启动引导提供"
        );
        assert_eq!(
            config.logging.dir,
            root.join("deps/logs"),
            "logging.dir 必须按 EXE 根目录解析为绝对路径"
        );
        assert!(
            config.state.playback_state_path.is_absolute(),
            "统一数据库路径必须解析为绝对路径"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn minimal_directory_with_only_bootstrap_yaml_initializes_full_config() {
        // 集成验证（与主程序启动链路一致）：目录中只有精简引导 config.yaml
        // （没有任何功能 yaml），BootstrapConfig::load → ConfigStore::open →
        // load_full → validate 全链路必须通过；新库自动以默认配置初始化，
        // load_full 的 http/logging 必须来自 bootstrap 注入值。
        let root =
            std::env::temp_dir().join(format!("config-store-minimal-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("config.yaml"),
            r#"
database_path: deps/data/playback.sqlite3
http:
  enabled: true
  host: 127.0.0.1
  port: 18889
  access_token: ""
logging:
  dir: deps/logs
  level: info
  rotate_daily: true
  retain_days: 7
"#,
        )
        .unwrap();

        let bootstrap =
            BootstrapConfig::load(&root.join("config.yaml"), &root).expect("加载最小启动配置");
        let store = ConfigStore::open(&bootstrap.database_path, &root, bootstrap.clone())
            .expect("打开统一配置数据库");
        let config = store.load_full().expect("加载完整配置");
        config
            .validate()
            .expect("最小引导目录初始化的配置必须通过完整校验");

        // load_full 的 http/logging 必须来自 bootstrap（端口 18889 非默认 18888）。
        assert_eq!(config.http.port, 18889, "http 段必须来自 bootstrap");
        assert_eq!(config.http.host, "127.0.0.1", "http 段必须来自 bootstrap");
        assert_eq!(config.logging.level, "info", "logging 段必须来自 bootstrap");
        assert_eq!(
            config.logging.dir,
            root.join("deps/logs"),
            "logging.dir 必须按 EXE 根目录解析为绝对路径"
        );
        assert_eq!(
            config.state.playback_state_path, bootstrap.database_path,
            "state.playback_state_path 必须注入统一数据库路径"
        );
        // 库文件确已在引导路径创建，且初始化只发生一次（revision 1）。
        assert!(
            bootstrap.database_path.exists(),
            "统一数据库文件必须在引导路径创建"
        );
        assert_eq!(
            store.current_revision().unwrap(),
            1,
            "初始化后 revision 必须为 1"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn bootstrap_load_parses_minimal_fields_and_resolves_relative_path() {
        let root = std::env::temp_dir().join(format!(
            "config-store-bootstrap-load-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        let config_path = root.join("config.yaml");
        fs::write(
            &config_path,
            r#"
database_path: deps/data/app.sqlite3
http:
  host: 127.0.0.1
  port: 18888
  enabled: true
  access_token: ""
logging:
  dir: deps/logs
  level: info
  rotate_daily: true
  retain_days: 7
"#,
        )
        .unwrap();

        let bootstrap = BootstrapConfig::load(&config_path, &root).unwrap();
        assert_eq!(
            bootstrap.database_path,
            root.join("deps/data/app.sqlite3"),
            "相对 database_path 必须相对 EXE 根目录解析为绝对路径"
        );
        assert_eq!(bootstrap.http.port, 18888, "http 段必须完整解析");
        assert_eq!(bootstrap.logging.level, "info", "logging 段必须完整解析");

        // 绝对路径保持不变。
        let absolute = std::env::temp_dir().join(format!(
            "config-store-bootstrap-abs-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&absolute).unwrap();
        let absolute_config = absolute.join("config.yaml");
        fs::write(
            &absolute_config,
            format!(
                "database_path: {}\nhttp: {{host: 127.0.0.1, port: 18888, enabled: true, access_token: \"\"}}\nlogging: {{dir: logs, level: info, rotate_daily: true, retain_days: 7}}\n",
                absolute.join("app.sqlite3").display()
            ),
        )
        .unwrap();
        let bootstrap = BootstrapConfig::load(&absolute_config, &absolute).unwrap();
        assert_eq!(
            bootstrap.database_path,
            absolute.join("app.sqlite3"),
            "绝对 database_path 必须保持不变"
        );

        // 字段缺失必须报错。
        fs::write(
            root.join("config-missing-db.yaml"),
            "http: {host: 127.0.0.1, port: 18888, enabled: true, access_token: \"\"}\nlogging: {dir: logs, level: info, rotate_daily: true, retain_days: 7}\n",
        )
        .unwrap();
        let error = BootstrapConfig::load(&root.join("config-missing-db.yaml"), &root).unwrap_err();
        assert!(
            error.to_string().contains("database_path"),
            "缺少 database_path 必须报错: {error}"
        );

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&absolute);
    }

    #[test]
    fn bootstrap_load_rejects_unknown_root_fields() {
        // 旧版完整 config.yaml 的多余段（如 window）必须被拒绝并附带引导说明，
        // 不能静默忽略（否则用户以为修改仍生效）。
        let root = std::env::temp_dir().join(format!(
            "config-store-bootstrap-reject-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        let config_path = root.join("config.yaml");
        fs::write(
            &config_path,
            r#"
database_path: deps/data/app.sqlite3
http:
  host: 127.0.0.1
  port: 18888
  enabled: true
  access_token: ""
logging:
  dir: deps/logs
  level: info
  rotate_daily: true
  retain_days: 7
window:
  target_process: yuanshen.exe
  content_width: 1920
  content_height: 1080
  auto_activate_window: false
  focus_point: { x: 1919, y: 1000 }
"#,
        )
        .unwrap();

        let error = BootstrapConfig::load(&config_path, &root).unwrap_err();
        assert!(
            error
                .chain()
                .any(|cause| cause.to_string().contains("window")),
            "未知顶层段必须被拒绝并报告段名: {error:#}"
        );
        assert!(
            error.to_string().contains("只保留") || error.to_string().contains("引导"),
            "解析失败必须附带引导段说明: {error}"
        );
        assert!(
            error
                .chain()
                .any(|cause| cause.to_string().contains("unknown field")),
            "原始解析错误必须保留在错误链中: {error:#}"
        );

        // 缺 database_path 时同样附带引导说明。
        fs::write(
            root.join("config-missing-db.yaml"),
            "http: {host: 127.0.0.1, port: 18888, enabled: true, access_token: \"\"}\nlogging: {dir: logs, level: info, rotate_daily: true, retain_days: 7}\n",
        )
        .unwrap();
        let error = BootstrapConfig::load(&root.join("config-missing-db.yaml"), &root).unwrap_err();
        assert!(
            error.to_string().contains("database_path"),
            "缺少 database_path 必须报错: {error}"
        );
        assert!(
            error.to_string().contains("只保留") || error.to_string().contains("引导"),
            "缺少 database_path 必须附带引导段说明: {error}"
        );

        let _ = fs::remove_dir_all(&root);
    }
}
