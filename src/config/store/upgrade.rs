//! 库级升级**编排**：备份 → 事务内读改写 → 更新 `config_meta.schema_version`。
//!
//! 迁移**规则**不在这里：字段改名/删除与旧文案改写全部由
//! [`crate::config::migrations`] 的统一管线提供（`migrate_sections` 结构阶段、
//! `normalize_content` 内容阶段）。本模块只回答四个工程问题：
//!
//! 1. **什么时候跑**——版本低于当前且有迁移路径时平滑迁移，没有路径时断代重置；
//! 2. **在哪个事务里跑**——`IMMEDIATE` 写事务，取锁后**重查版本**：另一个进程可能
//!    已完成升级，此时放弃本次并重走 [`ConfigStore::ensure_schema`]，绝不覆盖同版本
//!    保存的配置；
//! 3. **失败了怎么收场**——整笔事务回滚，原库保持旧版本原样，`open` 返回带上下文的
//!    错误；备份文件保留（它同时也是还原点）；
//! 4. **历史快照怎么办**——按同一份管线迁移，否则回滚到迁移前的版本会遇到
//!    「字段已不存在」；单条快照失败只告警保留原样，不阻断本次升级。
//!
//! 两条路径都先整库备份再改库（`VACUUM INTO`，含 `-wal` 里已提交的数据），且都只
//! 碰配置三表：歌曲缓存、`request_state`、命令回复表都不受影响。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rusqlite::OptionalExtension;
use serde_json::{Map, Value, json};

use crate::config::migrations;

use super::ConfigStore;
use super::sqlite::{CONFIG_SCHEMA_VERSION, now_ms, read_sections_from, read_snapshot_rows};
use super::tables::CONFIG_TABLES;

/// 每个配置库最多保留的 `.bak` 还原点份数。
///
/// 迁移每次打开旧库都会用 `VACUUM INTO` 留一份备份，而备份里含**明文 secret**
/// （`ai.api_key` 等）。旧实现只增不减，一次次的升级会把带密钥的副本无限铺在同一
/// 目录里；这里只保留最近的 [`BACKUP_RETENTION`] 份，更旧的删除。
///
/// 删除失败只告警不中断：迁移本身已经成功，不能因为清不动一个旧备份就让用户
/// 卡在「升级不了也回不去」的状态。备份排序按文件修改时间倒序，同一时间按
/// 文件名倒序（`unique_backup_path` 的 `-N` 后缀保证同名不冲突）。
const BACKUP_RETENTION: usize = 3;

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

impl ConfigStore {
    /// 旧配置断代：先备份整库，再取写锁重查版本，只重建配置三表。
    /// 缓存及其他表不受影响。
    pub(super) fn reset_previous_schema(
        connection: &mut rusqlite::Connection,
        path: &Path,
        stale_version: i64,
    ) -> Result<()> {
        // 备份必须在改库之前完成：断代会删掉旧配置，事后无法还原。
        // 备份失败只告警不阻断——否则用户会被卡在"升级不了也回不去"的死局里，
        // 而缓存与旧配置都还在原库里。
        match backup_before_schema_change(connection, path, stale_version) {
            Ok(Some(backup)) => log::warn!(
                "配置结构版本从 v{stale_version} 升级到 v{CONFIG_SCHEMA_VERSION}，需要重建配置表；旧库已备份到「{}」，可据此手工找回旧配置",
                backup.display()
            ),
            Ok(None) => {}
            Err(error) => log::warn!(
                "配置断代前的旧库备份失败（步骤：复制旧配置库），升级仍会继续，但旧配置无法还原：{error:#}"
            ),
        }
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let version: i64 = transaction.query_row(
            "SELECT schema_version FROM config_meta WHERE id = 1",
            [],
            |row| row.get(0),
        )?;
        if version >= CONFIG_SCHEMA_VERSION {
            // 另一进程可能已完成断代；重新校验，绝不覆盖同版本保存的配置。
            drop(transaction);
            return Self::ensure_schema(connection, path);
        }
        // B04-P1-6：取锁后版本必须仍等于决策时读到的版本。若已被别的进程推进到
        // 另一个旧版本，本次断代会按过期前提丢弃配置——放弃并重走 ensure_schema。
        if version != stale_version {
            log::warn!(
                "配置断代：取写锁后 schema 版本已从 v{stale_version} 变为 v{version}（另一进程刚升级过），放弃本次断代并重新校验"
            );
            drop(transaction);
            return Self::ensure_schema(connection, path);
        }
        transaction.execute_batch(
            "DROP TABLE config_revisions;
                 DROP TABLE config_sections;
                 DROP TABLE config_meta;",
        )?;
        Self::initialize_database_transaction(&transaction, now_ms() as i64)?;
        transaction.commit()?;
        log::warn!(
            "旧配置已重置为默认值（v{stale_version} → v{CONFIG_SCHEMA_VERSION}），用户此前的面板设置需要重新填写；歌曲缓存与音频文件未受影响：{}",
            path.display()
        );
        Ok(())
    }

    /// 旧配置平滑迁移：先备份整库，再取写锁重查版本，在同一事务内读 sections
    /// → 迁移 → 写回 sections → 更新 `config_meta.schema_version`。
    ///
    /// **不 DROP 配置表**：迁移只改被改名/删除的键，用户其余设置与歌曲缓存
    /// 原样保留。任何一步失败都会让事务回滚，原库保持不变并返回错误，不会半写。
    ///
    /// 历史快照（`config_revisions`）也按同一张映射表迁移；单条快照迁移失败
    /// 只告警保留原样（该版本回滚时会明确报字段不存在），不影响本次升级。
    pub(super) fn migrate_previous_schema(
        connection: &mut rusqlite::Connection,
        path: &Path,
        stale_version: i64,
    ) -> Result<()> {
        // 与断代同策略：先备份整库（VACUUM INTO 生成一致快照，含 -wal 已提交数据）。
        // 迁移本身不删数据，但改名/删除字段不可逆，仍要留还原点。
        let backup = match backup_before_schema_change(connection, path, stale_version) {
            Ok(Some(backup)) => {
                log::info!(
                    "配置结构版本从 v{stale_version} 升级到 v{CONFIG_SCHEMA_VERSION}，将按字段映射表平滑迁移；旧库已备份到「{}」",
                    backup.display()
                );
                Some(backup)
            }
            Ok(None) => None,
            Err(error) => {
                log::warn!(
                    "配置迁移前的旧库备份失败（步骤：复制旧配置库），迁移仍会继续，但旧值无法还原：{error:#}"
                );
                None
            }
        };
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let version: i64 = transaction.query_row(
            "SELECT schema_version FROM config_meta WHERE id = 1",
            [],
            |row| row.get(0),
        )?;
        if version >= CONFIG_SCHEMA_VERSION {
            // 另一进程可能已完成迁移或断代；重新校验，绝不覆盖同版本保存的配置。
            drop(transaction);
            return Self::ensure_schema(connection, path);
        }
        if version != stale_version {
            // 版本在取锁前被别的进程推进到另一个仍需迁移的旧版本：按实际版本重来。
            // 放弃而不是继续，否则会用 v{stale_version} 的映射表去改 v{version} 的数据
            // （并发升级可能把新版本数据回退，B04-P1-6）。
            log::warn!(
                "配置迁移：取写锁后 schema 版本已从 v{stale_version} 变为 v{version}（另一进程刚升级过），放弃本次迁移并重新校验"
            );
            drop(transaction);
            return Self::ensure_schema(connection, path);
        }
        let timestamp = now_ms() as i64;
        match Self::run_migration_transaction(&transaction, version, timestamp) {
            Ok(outcome) => {
                transaction.commit()?;
                log::info!(
                    "旧配置已平滑迁移到 v{CONFIG_SCHEMA_VERSION}（v{version} → v{CONFIG_SCHEMA_VERSION}）：{} 个段已改写、{} 个字段被改名或删除、{} 条历史快照已同步；用户其余设置、歌曲缓存与音频文件未受影响{}",
                    outcome.rewritten_sections,
                    outcome.changed_fields,
                    outcome.migrated_revisions,
                    backup
                        .as_ref()
                        .map(|backup| format!("；旧库备份「{}」", backup.display()))
                        .unwrap_or_default(),
                );
                Ok(())
            }
            Err(error) => {
                // transaction 在此处 drop：未提交的写入全部回滚，原库保持 v{version}。
                let backup_hint = backup
                    .as_ref()
                    .map(|backup| format!("，旧库备份在「{}」", backup.display()))
                    .unwrap_or_default();
                Err(error).with_context(|| {
                        format!(
                            "配置迁移 v{version} → v{CONFIG_SCHEMA_VERSION} 失败，已回滚本次事务，原配置保持不变{backup_hint}"
                        )
                    })
            }
        }
    }

    /// 在写事务内执行迁移，返回统计；任何错误都由调用方回滚整个事务。
    ///
    /// 收口在 `upgrade` 内部：迁移统计只用于本模块的日志，对外只暴露「失败会回滚」。
    fn run_migration_transaction(
        transaction: &rusqlite::Transaction,
        from_version: i64,
        timestamp: i64,
    ) -> Result<MigrationOutcome> {
        let revision: i64 =
            transaction.query_row("SELECT revision FROM config_meta WHERE id = 1", [], |row| {
                row.get(0)
            })?;
        let before = read_sections_from(transaction)?;
        // migrate_sections 以 BTreeMap 为契约（段顺序稳定、与 serde_json::Map 的
        // 排序语义一致）；读写库时再转回 serde_json::Map。
        let mut migrated: BTreeMap<String, Value> = before.clone().into_iter().collect();
        migrations::migrate_sections(from_version, &mut migrated)?;
        let sections: Map<String, Value> = migrated.into_iter().collect();
        let changed_fields = changed_field_count(&before, &sections);

        // 逐段写回；被剪掉的段直接删除，避免残留旧结构。
        for (section, value) in &sections {
            transaction.execute(
                    "INSERT OR REPLACE INTO config_sections (section, value_json, revision, updated_at_ms)
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![
                        section,
                        serde_json::to_string(value)?,
                        revision,
                        timestamp
                    ],
                )?;
        }
        for section in before.keys().filter(|key| !sections.contains_key(*key)) {
            transaction.execute(
                "DELETE FROM config_sections WHERE section = ?1",
                rusqlite::params![section],
            )?;
        }

        // 历史快照按同一张映射表迁移：否则回滚到迁移前的版本会遇到「字段已不存在」。
        // 单条快照迁移失败只告警保留原样，不阻断本次升级。
        let mut migrated_revisions = 0usize;
        for (snapshot_revision, snapshot) in read_snapshot_rows(transaction)? {
            let mut value: BTreeMap<String, Value> = match serde_json::from_str(&snapshot) {
                Ok(value) => value,
                Err(error) => {
                    log::warn!(
                        "配置迁移：历史快照 v{snapshot_revision} 不是可解析的段对象，保持原样（回滚到该版本会明确报错）：{error}"
                    );
                    continue;
                }
            };
            match migrations::migrate_sections(from_version, &mut value) {
                Ok(()) => {
                    transaction.execute(
                        "UPDATE config_revisions SET snapshot_json = ?1 WHERE revision = ?2",
                        rusqlite::params![
                            serde_json::to_string(&Value::Object(
                                value.into_iter().collect::<Map<String, Value>>()
                            ))?,
                            snapshot_revision
                        ],
                    )?;
                    migrated_revisions += 1;
                }
                Err(error) => log::warn!(
                    "配置迁移：历史快照 v{snapshot_revision} 迁移失败，保持原样（回滚到该版本会明确报错）：{error:#}"
                ),
            }
        }

        transaction.execute(
            "UPDATE config_meta SET schema_version = ?1, updated_at_ms = ?2 WHERE id = 1",
            rusqlite::params![CONFIG_SCHEMA_VERSION, timestamp],
        )?;
        Ok(MigrationOutcome {
            rewritten_sections: sections.len(),
            changed_fields,
            migrated_revisions,
        })
    }

    /// 内容阶段迁移：每次打开配置库执行 [`migrations::normalize_content`]，
    /// 仅在有真实差异时才开启写事务、写入变更段并推进 revision/快照。
    pub(super) fn apply_content_migrations(connection: &mut rusqlite::Connection) -> Result<()> {
        let mut probe_sections: BTreeMap<String, Value> =
            read_sections_from(connection)?.into_iter().collect();
        let probe = migrations::normalize_content(&mut probe_sections)?;
        if probe.changed_sections.is_empty() {
            return Ok(());
        }

        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut sections: BTreeMap<String, Value> =
            read_sections_from(&transaction)?.into_iter().collect();
        let report = migrations::normalize_content(&mut sections)?;
        if report.changed_sections.is_empty() {
            return Ok(());
        }

        let revision: i64 =
            transaction.query_row("SELECT revision FROM config_meta WHERE id = 1", [], |row| {
                row.get(0)
            })?;
        let next_revision = revision + 1;
        let timestamp = now_ms() as i64;

        for section_name in &report.changed_sections {
            if let Some(section_value) = sections.get(section_name) {
                transaction.execute(
                    "INSERT INTO config_sections (section, value_json, revision, updated_at_ms)
                         VALUES (?1, ?2, ?3, ?4)
                         ON CONFLICT(section) DO UPDATE SET
                             value_json = excluded.value_json,
                             revision = excluded.revision,
                             updated_at_ms = excluded.updated_at_ms",
                    rusqlite::params![
                        section_name,
                        serde_json::to_string(section_value)?,
                        next_revision,
                        timestamp
                    ],
                )?;
            } else {
                transaction.execute(
                    "DELETE FROM config_sections WHERE section = ?1",
                    rusqlite::params![section_name],
                )?;
            }
        }

        let snapshot: Map<String, Value> = sections.into_iter().collect();
        transaction.execute(
            "INSERT INTO config_revisions (revision, snapshot_json, created_at_ms)
                 VALUES (?1, ?2, ?3)",
            rusqlite::params![
                next_revision,
                serde_json::to_string(&Value::Object(snapshot))?,
                timestamp
            ],
        )?;
        transaction.execute(
            "UPDATE config_meta SET revision = ?1, updated_at_ms = ?2 WHERE id = 1",
            rusqlite::params![next_revision, timestamp],
        )?;
        transaction.execute(
            "DELETE FROM config_revisions WHERE revision <= ?1",
            rusqlite::params![next_revision - 50],
        )?;
        transaction.commit()?;
        log::info!(
            "配置内容迁移已完成：更新 {} 个配置段（步骤：{}）",
            report.changed_sections.len(),
            report.applied.join("、")
        );
        Ok(())
    }

    /// `config migrate --dry-run --json`：只读打开配置库，报告**将要**做的
    /// rename / prune / fill、以及备份保留清理，绝不写库、绝不创建备份。
    ///
    /// 返回 JSON（`Value`）而不是错误：CLI 需要在「库比程序新」这类拒绝场景
    /// 照样输出结构化报告并以非零码退出，而不是只吐一行错误。
    ///
    /// 预演复用真实的迁移管线（[`migrations::migrate_sections`] /
    /// [`migrations::normalize_content`]）在**内存副本**上跑一遍，因此不会出现
    /// 「预演说能迁、真迁却失败」的两套逻辑。原始 sections 只读不改。
    pub(crate) fn dry_run_migration_report(database_path: &Path) -> Result<Value> {
        let current_version = CONFIG_SCHEMA_VERSION;
        let mut report = json!({
            "dry_run": true,
            "database_path": database_path.display().to_string(),
            "current_program_version": current_version,
        });
        if !database_path.is_file() {
            report["outcome"] = json!("database_missing");
            report["plan"] = empty_migration_plan();
            report["error"] = json!({
                "kind": "database_missing",
                "message": format!("配置数据库不存在: {}", database_path.display()),
            });
            return Ok(report);
        }
        // 只读连接：dry-run 的任何分支都不写数据。不要调用
        // `initialize_connection`——把连接切到 WAL 本身就会改库文件头。
        let connection = rusqlite::Connection::open_with_flags(
            database_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| {
            format!(
                "以只读方式打开配置数据库失败（预演不写库）: {}",
                database_path.display()
            )
        })?;
        connection.pragma_update(None, "busy_timeout", 5_000i64)?;

        let mut missing_tables = Vec::new();
        for table in CONFIG_TABLES {
            let exists = connection
                .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?1")?
                .exists(rusqlite::params![table])?;
            if !exists {
                missing_tables.push(table);
            }
        }
        if !missing_tables.is_empty() {
            report["outcome"] = json!("invalid_database");
            report["plan"] = empty_migration_plan();
            report["error"] = json!({
                "kind": "missing_tables",
                "missing_tables": missing_tables,
                "message": format!("配置数据库缺少配置表「{}」，无法预演迁移", missing_tables.join("」「")),
            });
            return Ok(report);
        }
        let stored_version: Option<i64> = connection
            .query_row(
                "SELECT schema_version FROM config_meta WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let Some(stored_version) = stored_version else {
            report["outcome"] = json!("invalid_database");
            report["plan"] = empty_migration_plan();
            report["error"] = json!({
                "kind": "missing_meta",
                "message": "config_meta 缺少初始化数据，无法预演迁移",
            });
            return Ok(report);
        };
        report["stored_version"] = json!(stored_version);
        // 只有真要迁移/重置时才会新增备份并清理旧备份；已是最新或被拒绝时不碰备份。
        report["backup"] = backup_plan(database_path, stored_version, false);

        if stored_version > current_version {
            report["outcome"] = json!("refused_newer_database");
            report["plan"] = empty_migration_plan();
            report["error"] = json!({
                "kind": "newer_database",
                "message": format!("配置数据库 schema 版本高于当前程序（{stored_version} > {current_version}）"),
                "hint": "请升级程序或修复配置三表（不要删除含歌曲缓存的数据库）",
                "stored_version": stored_version,
                "current_program_version": current_version,
            });
            return Ok(report);
        }
        if stored_version == current_version {
            report["outcome"] = json!("up_to_date");
            report["plan"] = empty_migration_plan();
            return Ok(report);
        }
        if !migrations::has_migration_path(stored_version) {
            report["outcome"] = json!("would_reset");
            report["plan"] = empty_migration_plan();
            report["backup"] = backup_plan(database_path, stored_version, true);
            report["note"] = json!(format!(
                "v{stored_version} 没有字段映射表，打开时会断代重置为默认值（重置前会先备份整库）"
            ));
            return Ok(report);
        }

        let before = read_sections_from(&connection)?;
        let mut migrated: BTreeMap<String, Value> = before.clone().into_iter().collect();
        match migrations::migrate_sections(stored_version, &mut migrated) {
            Ok(()) => {
                let after: Map<String, Value> = migrated.into_iter().collect();
                let mut plan = migration_plan(&before, &after);
                // 内容阶段（旧文案改写）也会在 open 时跑；这里只报告会改哪些段。
                let mut content: BTreeMap<String, Value> = after.into_iter().collect();
                match migrations::normalize_content(&mut content) {
                    Ok(content_report) => {
                        plan["content_sections"] = json!(content_report.changed_sections);
                    }
                    Err(error) => {
                        plan["content_error"] = json!(format!("{error:#}"));
                    }
                }
                report["outcome"] = json!("would_migrate");
                report["plan"] = plan;
                report["backup"] = backup_plan(database_path, stored_version, true);
            }
            Err(error) => {
                report["outcome"] = json!("migration_would_fail");
                report["plan"] = empty_migration_plan();
                // 备份在迁移事务之前完成：失败也会留下还原点。
                report["backup"] = backup_plan(database_path, stored_version, true);
                report["error"] = json!({
                    "kind": "migration_would_fail",
                    "message": format!("配置迁移 v{stored_version} → v{current_version} 会在事务内失败，数据库不会被改动"),
                    "detail": format!("{error:#}"),
                });
            }
        }
        Ok(report)
    }

    /// `config migrate --dry-run --json` 的库侧入口：从 `config.yaml` 解析
    /// `database_path`，再走 [`Self::dry_run_migration_report`]。
    ///
    /// **不调用** `BootstrapConfig::load`：那条路径在 `access_token` 为空时会生成
    /// 令牌并**回写 config.yaml**，与「dry-run 不写任何东西」的约定冲突。这里只读
    /// 出预演需要的 `database_path` 一个字段，其余引导段不解析。
    pub(crate) fn dry_run_migration_report_from_config(config_path: &Path) -> Result<Value> {
        #[derive(serde::Deserialize)]
        struct DryRunBootstrap {
            database_path: PathBuf,
        }
        let text = fs::read_to_string(config_path)
            .with_context(|| format!("读取启动配置失败: {}", config_path.display()))?;
        let bootstrap: DryRunBootstrap = serde_norway::from_str(&text).with_context(|| {
            format!(
                "解析启动配置失败: {}（预演只需要 database_path，且不会改写 config.yaml）",
                config_path.display()
            )
        })?;
        if bootstrap.database_path.as_os_str().is_empty() {
            bail!(
                "config.yaml 的 database_path 不能为空: {}",
                config_path.display()
            );
        }
        let executable_root = config_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("配置路径缺少父目录: {}", config_path.display()))?;
        let database_path = if bootstrap.database_path.is_relative() {
            executable_root.join(&bootstrap.database_path)
        } else {
            bootstrap.database_path
        };
        Self::dry_run_migration_report(&database_path)
    }
}

/// 空迁移计划（已是最新/拒绝预演时也要给调用方稳定的 JSON 形状）。
fn empty_migration_plan() -> Value {
    json!({
        "renames": [],
        "pruned": [],
        "filled": [],
        "updated": [],
    })
}

/// 比较迁移前后的 sections，产出 rename / prune / fill / updated 四类动作。
///
/// 只递归对象（配置迁移的改名/删除都发生在段内对象里）；数组与标量整体比较。
/// rename 的判据是「同一父路径下，删掉的键与新增的键值完全相等」——这正是
/// `migrations` 里按键搬运旧值的语义，不需要在这里再抄一份映射表。两边同时
/// 存在时新键胜出、旧键被删，此时新增集合里没有它，旧键会如实归入 prune。
fn migration_plan(before: &Map<String, Value>, after: &Map<String, Value>) -> Value {
    let mut removed: Vec<(String, Value)> = Vec::new();
    let mut added: Vec<(String, Value)> = Vec::new();
    let mut updated: Vec<String> = Vec::new();
    for (section, before_value) in before {
        match after.get(section) {
            Some(after_value) => diff_sections(
                before_value,
                after_value,
                section,
                &mut removed,
                &mut added,
                &mut updated,
            ),
            None => removed.push((section.clone(), before_value.clone())),
        }
    }
    for (section, after_value) in after {
        if !before.contains_key(section) {
            added.push((section.clone(), after_value.clone()));
        }
    }
    removed.sort_by(|left, right| left.0.cmp(&right.0));
    added.sort_by(|left, right| left.0.cmp(&right.0));
    updated.sort();

    let mut used_added = vec![false; added.len()];
    let mut renames = Vec::new();
    let mut pruned = Vec::new();
    for (path, value) in &removed {
        let is_section = !path.contains('.');
        let (parent, key) = split_section_path(path);
        let mut renamed = false;
        if !is_section {
            for (index, (added_path, added_value)) in added.iter().enumerate() {
                if used_added[index] || !added_path.contains('.') || added_value != value {
                    continue;
                }
                let (added_parent, added_key) = split_section_path(added_path);
                if added_parent != parent {
                    continue;
                }
                used_added[index] = true;
                renamed = true;
                renames.push(json!({
                    "section": parent,
                    "from": key,
                    "to": added_key,
                }));
                break;
            }
        }
        if !renamed {
            pruned.push(json!({
                "section": if is_section { Value::Null } else { json!(parent) },
                "key": key,
                "value": value,
                "kind": if is_section { "section" } else { "field" },
            }));
        }
    }
    let filled: Vec<Value> = added
        .iter()
        .enumerate()
        .filter(|(index, _)| !used_added[*index])
        .map(|(_, (path, value))| {
            let is_section = !path.contains('.');
            let (parent, key) = split_section_path(path);
            json!({
                "section": if is_section { Value::Null } else { json!(parent) },
                "key": key,
                "value": value,
                "kind": if is_section { "section" } else { "field" },
            })
        })
        .collect();
    json!({
        "renames": renames,
        "pruned": pruned,
        "filled": filled,
        "updated": updated,
    })
}

/// 递归比较同一段内的对象；路径用点号拼接（`templates.friend_confirm`）。
fn diff_sections(
    before: &Value,
    after: &Value,
    path: &str,
    removed: &mut Vec<(String, Value)>,
    added: &mut Vec<(String, Value)>,
    updated: &mut Vec<String>,
) {
    match (before, after) {
        (Value::Object(before), Value::Object(after)) => {
            for (key, before_value) in before {
                let child = format!("{path}.{key}");
                match after.get(key) {
                    Some(after_value) => {
                        diff_sections(before_value, after_value, &child, removed, added, updated)
                    }
                    None => removed.push((child, before_value.clone())),
                }
            }
            for (key, after_value) in after {
                if !before.contains_key(key) {
                    added.push((format!("{path}.{key}"), after_value.clone()));
                }
            }
        }
        (before, after) if before != after => updated.push(path.to_string()),
        _ => {}
    }
}

/// `templates.friend_confirm` → (`templates`, `friend_confirm`)；顶层段返回 `("", 段名)`。
fn split_section_path(path: &str) -> (&str, &str) {
    path.rsplit_once('.').unwrap_or(("", path))
}

/// 预演本库的备份保留结果：dry-run 会新增 1 份（最新），因此保留现有最新的
/// `BACKUP_RETENTION - 1` 份，更旧的会被删除（这里只报告，不真删）。
fn backup_plan(database_path: &Path, stale_version: i64, would_back_up: bool) -> Value {
    let existing = matching_backups(database_path);
    // 真要迁移时新备份会占一个名额，因此只保留现有最新的 `retention - 1` 份；
    // 不迁移时打开不会碰备份，`would_create` / `would_remove` 都必须是空。
    let keep_existing = if would_back_up {
        BACKUP_RETENTION.saturating_sub(1)
    } else {
        BACKUP_RETENTION
    };
    let would_remove: Vec<String> = if would_back_up {
        existing
            .iter()
            .skip(keep_existing)
            .map(|(_, path)| backup_file_name(path))
            .collect()
    } else {
        Vec::new()
    };
    json!({
        "retention": BACKUP_RETENTION,
        "existing": existing.len(),
        "would_create": if would_back_up {
            json!(backup_file_name(&backup_path(database_path, stale_version)))
        } else {
            Value::Null
        },
        "would_remove": would_remove,
    })
}

/// 备份文件的文件名（只用于报告；非 UTF-8 文件名按 lossy 呈现）。
fn backup_file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// 改 schema 前把整库备份到同目录的 `.bak` 文件，返回备份路径。
///
/// 断代重置与平滑迁移共用：两者都会不可逆地改掉旧配置，先留还原点。
///
/// 用 `VACUUM INTO` 而不是复制文件：库开着 WAL，直接复制主库文件会漏掉
/// 还在 `-wal` 里没 checkpoint 的已提交数据；`VACUUM INTO` 由 SQLite 在
/// 同一连接上生成一致快照。
///
/// 返回 `Ok(None)` 表示库文件还不存在（全新库，没有可备份的内容）。
/// 备份名带旧 schema 版本与时间戳，重复升级不会覆盖上一次的备份。
fn backup_before_schema_change(
    connection: &rusqlite::Connection,
    path: &Path,
    stale_version: i64,
) -> Result<Option<PathBuf>> {
    if !path.is_file() {
        return Ok(None);
    }
    let backup = backup_path(path, stale_version);
    // 同名文件已存在时换个名字，绝不覆盖用户已有的备份。
    let backup = unique_backup_path(backup);
    // VACUUM INTO 的目标必须是尚不存在的文件；参数化避免路径里的引号破坏 SQL。
    connection.execute(
        "VACUUM INTO ?1",
        rusqlite::params![backup.display().to_string()],
    )?;
    // 备份里含明文 secret：留够了就删掉更旧的，避免一次次的升级把带密钥的副本
    // 无限铺在同一目录。删除失败只告警（见 prune_stale_backups）。
    prune_stale_backups(path);
    Ok(Some(backup))
}

/// 清理同一配置库超过 [`BACKUP_RETENTION`] 份的旧 `.bak`，返回实际删除份数。
///
/// 只删**本库**的备份：文件名必须以 `<库名>.v` 开头、以 `.bak` 结尾，目录里
/// 其它库或用户手工放的备份一律不碰。排序按修改时间倒序（新→旧），时间相同
/// 时按文件名倒序；拿不到 mtime 的文件按最旧处理。
///
/// 单个文件删除失败（占用、权限、竞态消失）只 `log::warn!` 并继续删下一个：
/// 迁移已经成功，清理是尽力而为的磁盘维护，不能反过来让升级失败。
fn prune_stale_backups(database_path: &Path) -> usize {
    let backups = matching_backups(database_path);
    let mut removed = 0usize;
    for (_, backup) in backups.into_iter().skip(BACKUP_RETENTION) {
        match fs::remove_file(&backup) {
            Ok(()) => removed += 1,
            Err(error) => log::warn!(
                "清理旧配置备份失败（步骤：删除超过保留上限 {BACKUP_RETENTION} 份的旧 .bak；本次升级继续）：{}: {error}",
                backup.display()
            ),
        }
    }
    if removed > 0 {
        log::info!(
            "已清理 {removed} 份超过保留上限的旧配置备份（每个库最多保留最近 {BACKUP_RETENTION} 份，备份含明文密钥请妥善保管）"
        );
    }
    removed
}

/// 列出同一配置库的 `.bak` 备份，按修改时间从新到旧排序。
///
/// 文件名匹配 `<库名>.v<旧版本>.<时间戳>.bak` / `<...>-N.bak`；`read_dir` 出错
/// 或目录不可读时返回空列表（调用方把「没有备份」与「读不到目录」都当成无需清理）。
fn matching_backups(database_path: &Path) -> Vec<(SystemTime, PathBuf)> {
    let Some(directory) = database_path.parent() else {
        return Vec::new();
    };
    let Some(stem) = database_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
    else {
        return Vec::new();
    };
    let prefix = format!("{stem}.v");
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut backups: Vec<(SystemTime, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if !file_name.starts_with(&prefix) || !file_name.ends_with(".bak") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
        backups.push((modified, entry.path()));
    }
    backups.sort_by(|(left_time, left_path), (right_time, right_path)| {
        right_time
            .cmp(left_time)
            .then_with(|| right_path.cmp(left_path))
    });
    backups
}

/// 备份文件名：`<库名>.v<旧版本>.YYYYmmdd-HHMMSS.bak`。
fn backup_path(path: &Path, stale_version: i64) -> PathBuf {
    let stamp = backup_timestamp();
    let stem = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config".to_string());
    path.with_file_name(format!("{stem}.v{stale_version}.{stamp}.bak"))
}

/// 若目标已存在则追加 `-1`、`-2`…，保证不覆盖既有备份。
fn unique_backup_path(candidate: PathBuf) -> PathBuf {
    if !candidate.exists() {
        return candidate;
    }
    let base = candidate.with_extension("");
    for index in 1..1000 {
        let next = PathBuf::from(format!("{}-{index}.bak", base.display()));
        if !next.exists() {
            return next;
        }
    }
    candidate
}

/// 文件名安全的时间戳（`YYYYmmdd-HHMMSS`，本地 +08:00）。
fn backup_timestamp() -> String {
    crate::adapters::logging::format_timestamp_compact(SystemTime::now())
}

/// 比较迁移前后的 sections，统计值发生变化的字段点路径数（只用于日志）。
///
/// 段内递归比较：对象按键递归、数组与标量整体比较；新增/删除的键各计一处。
fn changed_field_count(before: &Map<String, Value>, after: &Map<String, Value>) -> usize {
    fn walk(before: Option<&Value>, after: Option<&Value>) -> usize {
        match (before, after) {
            (Some(Value::Object(before)), Some(Value::Object(after))) => {
                let mut count = before
                    .keys()
                    .filter(|key| !after.contains_key(*key))
                    .count()
                    + after
                        .keys()
                        .filter(|key| !before.contains_key(*key))
                        .count();
                for (key, before_value) in before {
                    if let Some(after_value) = after.get(key) {
                        count += walk(Some(before_value), Some(after_value));
                    }
                }
                count
            }
            (Some(before), Some(after)) => usize::from(before != after),
            (None, None) => 0,
            _ => 1,
        }
    }
    let mut count = before
        .keys()
        .filter(|key| !after.contains_key(*key))
        .count()
        + after
            .keys()
            .filter(|key| !before.contains_key(*key))
            .count();
    for (section, before_value) in before {
        if let Some(after_value) = after.get(section) {
            count += walk(Some(before_value), Some(after_value));
        }
    }
    count
}

/// W5 迁移加固的回归测试：备份保留、v3 合成夹具、dry-run 与「库比程序新」。
#[cfg(test)]
#[path = "upgrade_tests.rs"]
mod upgrade_tests;
