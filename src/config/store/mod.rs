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
        // B04-P1-5：合并基准必须在事务内读（见 `save_merged_transactionally`），
        // 否则两个并发保存会基于同一份旧配置互相覆盖（丢更新）。
        let executable_root = self.executable_root.clone();
        self.save_merged_transactionally(base_revision, |current_sections| {
            // 应用候选：并集合并、剔除注入段、secret 空值/掩码保留、
            // `{"__clear__": true}` 标记强制清除（写入空串，绕过保留规则）。
            let mut merged = merge_sections(current_sections, &sections);
            let mut forced_cleared = BTreeSet::new();
            apply_secret_clear_markers(&mut merged, &mut forced_cleared);
            retain_secret_fields(&mut merged, current_sections, &forced_cleared);
            normalize_path_fields(&mut merged, &executable_root, false);
            merged
        })
    }

    /// 事务内的公共写入路径：在 `BEGIN IMMEDIATE` 之后读取合并基准，由 `prepare`
    /// 基于**同一事务视图**的 sections 产出待写入的完整配置，再校验、比对
    /// `base_revision`、写段 + 快照 + meta。
    ///
    /// 合并基准留在事务内是并发正确性的前提：读到旧基准再取写锁，两个并发保存
    /// 会各自覆盖对方刚提交的段（B04-P1-5）。校验失败时事务 drop 回滚，不写任何行。
    fn save_merged_transactionally<F>(
        &mut self,
        base_revision: u64,
        prepare: F,
    ) -> std::result::Result<ConfigSaveOutcome, ConfigSaveError>
    where
        F: FnOnce(&Map<String, Value>) -> Map<String, Value>,
    {
        // Immediate：立即取写锁，避免 HTTP 并发保存时 deferred 事务互相升级
        // 造成 SQLite busy 死锁（busy_timeout=5000 已有）。
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current_sections = read_sections_from(&transaction)?;
        let normalized_current = merge_sections(&current_sections, &Map::new());
        let mut merged = prepare(&current_sections);
        // B04-P1-4：落库前同步清理废弃命令覆盖项。只在下面的类型化 `config` 上
        // sanitize 会让 `merged`（真正写进 config_sections 与历史快照的那份）继续
        // 保留死条目，导致「读得进、存不回」：load_full 丢掉后，用户一保存又写回去。
        sanitize_deprecated_overrides_in_merged(&mut merged);
        // 组装完整配置并校验；失败返回字段级错误，不写库（事务 drop 即回滚）。
        let mut config = match AppConfig::from_db_value(
            Value::Object(merged.clone()),
            &self.bootstrap,
            &self.executable_root,
        ) {
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
        let timestamp = now_ms() as i64;
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
    /// 防止历史快照中的掩码或旧值覆盖当前密钥），走与 [`Self::save`] 相同的
    /// 校验/事务路径，记录新版本（当前 +1）。
    ///
    /// B04-P1-6：合并基准（当前 sections 的 secret 值）与写回必须在**同一个**
    /// 事务里读。既有实现先读基准、再调 `save()`（`save()` 会再读一次），
    /// 两次读取之间若有并发保存，回滚会把别人刚提交的密钥回退成旧快照的值。
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
        // 这一步只做结构预检（不碰库），与 save 的快速失败语义一致。
        self.build_config(&sections)?;
        // 合并基准由事务内读取的 current_sections 提供，不再经由 save() 二次读库。
        self.save_merged_transactionally(base_revision, move |current_sections| {
            force_retain_secret_fields(&mut sections, current_sections);
            sections
        })
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

/// 从待落库的 sections 里删掉已废弃命令的覆盖项（B04-P1-4）。
///
/// 与 `CommandAccessConfig::sanitize_deprecated_overrides`（类型化配置）同源：
/// 命令 id 清单只维护一份，这里只做 JSON 键删除，不重写其它字段，保证落库内容与
/// `load_full` / `LiveConfigs::apply` 实际使用的一致。
fn sanitize_deprecated_overrides_in_merged(merged: &mut Map<String, Value>) {
    let Some(overrides) = merged
        .get_mut("command_access")
        .and_then(Value::as_object_mut)
        .and_then(|section| section.get_mut("overrides"))
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    for id in crate::features::command_access::DEPRECATED_POLICY_IDS {
        if overrides.remove(*id).is_some() {
            log::warn!(
                "命令权限配置里的「{}」已废弃并忽略：点歌已拆分为五个平台命令（@QQ点歌/@网易点歌/@酷狗点歌/@B站点歌/@AI点歌），保存时已同步从配置库移除，请改用对应的平台条目",
                id
            );
        }
    }
}

/// B04-P1-4 的回归测试放在 `mod.rs` 内联：本批修复只允许改三个文件，
/// 不改同目录的 `tests.rs`。测试仍用真实临时 SQLite 文件与独立连接读库。
#[cfg(test)]
mod save_deprecated_override_tests {
    use super::*;
    use serde_json::json;

    fn temp_database(name: &str) -> (PathBuf, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("config-store-{name}-{}", uuid::Uuid::new_v4()));
        let database_path = root.join("deps/data/playback.sqlite3");
        std::fs::create_dir_all(database_path.parent().unwrap()).unwrap();
        (root, database_path)
    }

    fn bootstrap(database_path: &Path) -> BootstrapConfig {
        BootstrapConfig {
            database_path: database_path.to_path_buf(),
            http: crate::config::HttpConfig::default(),
            logging: crate::config::LoggingConfig::default(),
        }
    }

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

    #[test]
    fn save_drops_deprecated_song_override_from_persisted_sections() {
        // 只在类型化 config 上 sanitize 时，`load_full` 会丢掉 `@点歌`，
        // 但保存又把这条死配置写回 config_sections/历史快照。
        let (root, database_path) = temp_database("save-drops-deprecated-override");
        let mut store =
            ConfigStore::open(&database_path, &root, bootstrap(&database_path)).unwrap();
        let legacy = json!({
            "overrides": {
                "@点歌": {"enabled": false, "minimum_role": "admin"},
                "@QQ点歌": {"enabled": true, "minimum_role": "user"}
            }
        });
        store
            .connection
            .execute(
                "UPDATE config_sections SET value_json = ?1 WHERE section = 'command_access'",
                rusqlite::params![serde_json::to_string(&legacy).unwrap()],
            )
            .unwrap();
        let revision = store.current_revision().unwrap();
        // 空候选段 = 把当前库原样写回；落库仍须清掉已废弃条目。
        store.save(revision, Map::new()).unwrap();
        drop(store);

        let stored = stored_section(&database_path, "command_access");
        let overrides = stored["overrides"]
            .as_object()
            .expect("command_access.overrides 必须是对象");
        assert!(
            !overrides.contains_key("@点歌"),
            "落库内容不得保留已废弃命令覆盖项: {stored}"
        );
        assert!(
            overrides.contains_key("@QQ点歌"),
            "同段的其它有效条目必须原样保留: {stored}"
        );
        let snapshot = latest_snapshot(&database_path);
        assert!(
            !snapshot["command_access"]["overrides"]
                .as_object()
                .expect("历史快照里的 overrides 必须是对象")
                .contains_key("@点歌"),
            "历史快照同样必须是清理后的内容: {snapshot}"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
