//! SQLite 配置中心：`ConfigStore` 与三张配置表。
//!
//! 配置三表（config_meta / config_sections / config_revisions）与既有表
//! （request_state、cached_*、命令回复两表）共存于统一数据库；配置 schema 版本
//! （[`CONFIG_SCHEMA_VERSION`]）与请求状态快照版本、缓存 user_version 完全独立。
//!
//! 启动流程读取完整配置，Web 配置中心负责保存、历史版本和回滚。
//!
//! # 目录布局
//!
//! 本文件是**核心 API 与重导出**：`ConfigStore` 的字段与生命周期入口在这里，方法体
//! 按职责落到同目录子模块（同一个类型的多个 `impl` 块）。对外路径不变：调用方仍是
//! `crate::config::ConfigStore`（父模块 [`crate::config`] 统一重导出）。
//!
//! - [`types`]：保存/回滚结果、字段级错误、历史版本信息与错误枚举；
//! - [`sqlite`]：连接参数、WAL/锁语义、三表初始化与升级路径分派；
//! - [`tables`]：三表期望列与结构/约束校验（含 CHECK 表达式词法扫描）；
//! - [`upgrade`]：库级升级编排（备份 → 事务内读改写 → 版本号）；**迁移规则**在
//!   [`crate::config::migrations`]；
//! - [`replies`]：命令回复模板的独立 revision 与 CAS 保存；
//! - [`codec`]：sections ↔ `AppConfig`、路径/密钥规范化、脱敏与差异比较；
//! - `tests`：库级集成测试（真实临时 SQLite 文件）。
//!
//! # 打开配置库时发生什么
//!
//! ```text
//! open(path, root, bootstrap)
//! ├─ 建父目录 → 打开连接（有限重试，WAL/busy_timeout/foreign_keys）
//! ├─ ensure_schema：三态判定 + 逐列/约束校验 + 版本分派
//! │  ├─ 三表全不在            → 以 AppConfig::default() 初始化（revision 1）
//! │  ├─ 版本 == 当前           → 什么都不做
//! │  ├─ 版本 >  当前           → 明确失败（降级运行不许静默覆盖新版本配置）
//! │  └─ 版本 <  当前           → 有迁移路径：平滑迁移；没有：断代重置（都先备份）
//! ├─ ensure_reply_schema：命令回复两表（与配置 revision 独立）
//! └─ apply_content_migrations：旧文案改写；无差异则不写库、不涨 revision
//! ```
//!
//! 可见性口径：迁移只经 [`ConfigStore::open`] 这一个入口发生，迁移统计
//! （`upgrade::MigrationOutcome`）收在 [`upgrade`] 内部，不外泄到其它子模块。
mod codec;
mod replies;
mod sqlite;
mod tables;
mod types;
mod upgrade;

#[cfg(test)]
mod tests;

pub(crate) use codec::{SECRET_MASK, mask_secrets};
pub(crate) use sqlite::{CONFIG_SCHEMA_VERSION, SharedConfigStore};
pub(crate) use types::{
    ConfigFieldError, ConfigRevisionInfo, ConfigSaveError, ConfigSaveOutcome, StoredReply,
};
// `ConfigFieldErrors` 只作为 [`ConfigSaveError::Other`] 的 downcast 目标被测试取用，
// 生产路径不直接构造它，因此只在测试构建里重导出。
#[cfg(test)]
pub(crate) use types::ConfigFieldErrors;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use rusqlite::OptionalExtension;
use serde_json::{Map, Value};

use super::AppConfig;
use super::BootstrapConfig;

use codec::{
    apply_secret_clear_markers, collect_changed_paths, force_retain_secret_fields, merge_sections,
    normalize_path_fields, retain_secret_fields,
};
use sqlite::{now_ms, read_sections_from};
use types::{field_errors_from_anyhow, field_errors_from_message};

/// 配置持久化存储：config_sections 按段保存，config_meta 记录当前版本，
/// config_revisions 保存每次提交的完整快照（支持回滚）。
#[derive(Debug)]
pub struct ConfigStore {
    connection: rusqlite::Connection,
    executable_root: PathBuf,
    bootstrap: BootstrapConfig,
}

impl ConfigStore {
    /// 打开（必要时创建）配置数据库。
    ///
    /// 库不存在时创建三表并以 [`AppConfig::default()`] 初始化完整配置
    /// （分 section 写入 config_sections、meta 记 revision=1、写入 revision 1 快照）；
    /// 有迁移路径的旧配置平滑迁移（保留用户设置）；没有迁移路径的更老版本
    /// 重置为新默认值；未来版本或损坏的表结构明确失败。
    /// 库中已有 request_state / cached_* 等其他表不视为冲突。
    ///
    /// 打开是配置库**唯一**的迁移入口：结构迁移在 `ensure_schema` 内完成，内容迁移
    /// （旧文案改写）在最后补一遍。`load_full` 等只读路径一律不再迁移。
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
        let mut connection = Self::open_connection_with_retries(database_path)?;
        Self::ensure_schema(&mut connection, database_path)?;
        Self::ensure_reply_schema(&connection)?;
        Self::apply_content_migrations(&mut connection)?;
        Ok(Self {
            connection,
            executable_root: executable_root.to_path_buf(),
            bootstrap,
        })
    }
}

impl ConfigStore {
    /// 加载完整业务配置：读全部 config_sections 组装 JSON，注入启动引导
    /// （http/logging/state.playback_state_path），解析相对路径并通过完整校验。
    ///
    /// **不做 schema 迁移**：迁移在 [`Self::open`] 的 `ensure_schema` 里已经完成，
    /// 这里再迁移会重复改库、也会让只读路径（如 HTTP 展示）产生写入。
    pub fn load_full(&self) -> Result<AppConfig> {
        let sections = self.read_all_sections()?;
        let mut config = AppConfig::from_db_value(
            Value::Object(sections),
            &self.bootstrap,
            &self.executable_root,
        )
        .context("从配置数据库组装完整配置失败")?;
        // 顺序不能反：先丢弃已废弃的面板条目（如 `@点歌`），再校验。
        // 反过来的话，老配置库里一条早就不生效的死条目会让整个程序启动失败。
        config.command_access.sanitize_deprecated_overrides();
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
        let sections = self.read_all_sections()?;
        self.build_config(&sections)?;
        let mut value = Value::Object(sections);
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
        if let Some(object) = value.as_object_mut() {
            // 库里的旧值可能是混合分隔符甚至 root 下的绝对路径：显示前统一成
            // 原生分隔符，并把 root 下的绝对路径回落为相对（与默认值语义一致）。
            normalize_path_fields(object, &self.executable_root, true);
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
        let current_sections = self.read_all_sections()?;
        self.build_config(&current_sections)?;
        let mut merged = merge_sections(&current_sections, sections);

        // 与 save 同一路径：先应用清除标记，再保留其余 secret（保证预检结果
        // 与最终保存一致，清除标记不会被误判为非法类型）。
        let mut forced_cleared = BTreeSet::new();
        apply_secret_clear_markers(&mut merged, &mut forced_cleared);
        retain_secret_fields(&mut merged, &current_sections, &forced_cleared);
        normalize_path_fields(&mut merged, &self.executable_root, false);
        let mut config = match self.build_config(&merged) {
            Ok(config) => config,
            Err(error) => return Ok(field_errors_from_message(&error.to_string()).errors),
        };
        // 与 load_full / LiveConfigs::apply 同一口径：预检阶段也先丢弃已废弃的
        // 面板条目，否则预检会比实际保存更严——用户什么都没改，点保存却报
        // 「未知命令ID: @点歌」。
        config.command_access.sanitize_deprecated_overrides();
        match config.validate() {
            Ok(()) => Ok(Vec::new()),
            Err(error) => Ok(field_errors_from_anyhow(&error).errors),
        }
    }

    /// 用候选 sections 组装完整配置，但**不写库、不 bump revision**。
    ///
    /// 与 [`ConfigStore::save`] 同一合并/校验路径，供装配预览使用；
    /// 合并基准是当前库配置，未提交的段沿用已保存值。
    pub fn preview_candidate(&self, sections: &Map<String, Value>) -> anyhow::Result<AppConfig> {
        let current_sections = self.read_all_sections()?;
        let mut merged = merge_sections(&current_sections, sections);
        let mut forced_cleared = BTreeSet::new();
        apply_secret_clear_markers(&mut merged, &mut forced_cleared);
        retain_secret_fields(&mut merged, &current_sections, &forced_cleared);
        normalize_path_fields(&mut merged, &self.executable_root, false);
        let mut config = self.build_config(&merged)?;
        config.command_access.sanitize_deprecated_overrides();
        config.validate()?;
        Ok(config)
    }

    /// 保存配置：候选段整段替换（并集覆盖），完整校验通过后单事务写入。
    ///
    /// `sections` 为**整段替换**语义：提交段覆盖该段全部字段（提交段中缺失的
    /// 字段视为删除），未提交段保持原值；Web 表单必须按段提交完整值。
    ///
    /// secret 字段（ai.api_key 等）提交值为 null、空字符串或 [`SECRET_MASK`]
    /// （Web 页面回传的掩码）时保留当前值；提交值等于 JSON 对象
    /// `{"__clear__": true}`（`codec::SECRET_CLEAR_MARKER`）时清除该字段（写入空串）；
    /// base_revision 与库中当前版本不一致时拒绝写入。
    pub fn save(
        &mut self,
        base_revision: u64,
        sections: Map<String, Value>,
    ) -> std::result::Result<ConfigSaveOutcome, ConfigSaveError> {
        // 应用候选：并集合并、剔除注入段、secret 空值/掩码保留、
        // `{"__clear__": true}` 标记强制清除（写入空串，绕过保留规则）。
        let current_sections = self.read_all_sections()?;
        self.build_config(&current_sections)?;
        let normalized_current = merge_sections(&current_sections, &Map::new());
        let mut merged = merge_sections(&current_sections, &sections);
        let mut forced_cleared = BTreeSet::new();
        apply_secret_clear_markers(&mut merged, &mut forced_cleared);
        retain_secret_fields(&mut merged, &current_sections, &forced_cleared);
        normalize_path_fields(&mut merged, &self.executable_root, false);
        // 组装完整配置并校验；失败返回字段级错误，不写库。
        let mut config = match self.build_config(&merged) {
            Ok(config) => config,
            Err(error) => {
                return Err(ConfigSaveError::Other(
                    field_errors_from_message(&error.to_string()).into(),
                ));
            }
        };
        // 与 load_full 同一口径：落库前也丢弃废弃条目，避免「读得进、存不回」。
        config.command_access.sanitize_deprecated_overrides();
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
        // 历史快照必须完整符合当前结构，不用当前段或默认值补齐。
        self.build_config(&sections)?;
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
        read_sections_from(&self.connection)
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
