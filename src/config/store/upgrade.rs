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

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde_json::{Map, Value};

use std::collections::BTreeMap;

use crate::config::migrations;

use super::ConfigStore;
use super::sqlite::{CONFIG_SCHEMA_VERSION, now_ms, read_sections_from, read_snapshot_rows};

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
    Ok(Some(backup))
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
