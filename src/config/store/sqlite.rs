//! SQLite 连接与三表初始化：连接参数、锁语义、升级路径分派。
//!
//! 三条硬约束（都是实机踩过的）：
//!
//! - `busy_timeout` 必须在 `journal_mode=WAL` **之前**设置：切换 WAL 要独占锁，
//!   两个进程并发首次打开同一新库时后到者要能等待，否则立即 database is locked；
//! - 连接打开再做有限重试（[`OPEN_CONNECTION_RETRIES`]）：SQLite 头初始化阶段的
//!   锁竞争发生在 busy handler 存在之前，只能靠重试吸收；
//! - 三表判定是**三态**的：全不存在才初始化，全存在才逐列校验，只存在一部分明确
//!   失败——绝不把半初始化/损坏库当新库重建（那会把已有缓存一起当新库覆盖）。
//!
//! 本模块只负责「打开 + 校验 + 选路径」，迁移编排在 [`super::upgrade`]，
//! 表结构期望与 CHECK 校验在 [`super::tables`]。

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rusqlite::OptionalExtension;

use serde_json::{Map, Value};

use crate::config::AppConfig;
use crate::config::migrations;

use super::ConfigStore;
use super::tables::{
    CONFIG_TABLES, META_COLUMNS, REVISIONS_COLUMNS, SECTIONS_COLUMNS,
    config_meta_constraints_match, table_structure_matches,
};

/// 配置数据库 schema 版本；与请求状态快照版本及缓存 user_version 完全独立。
///
/// 7.0.0 起回复文案统一由命令面板持有（[<code>command_replies</code>] 表），
/// 配置表里的 `replies` 段被移除；同时命令回复目录重组为
/// key→命令 的归属表。这些变化让旧配置失去意义，因此提升版本号触发断代重置。
/// 正式版从 v6.2.0 的 1 只升一级到 2；开发期中间版本号（3/4）不再沿用，
/// 避免正式版与开发版共用同一版本号造成语义混淆。
///
/// 重置只重建配置三表，歌曲缓存与音频文件不受影响；重置前会先把整库
/// 备份到同目录的 `.bak` 文件，便于用户找回旧配置。
///
/// v2 → v3：AI 聊天记忆改成三层结构，新增单人中期摘要预算、大厅原文预算、
/// 大厅中期摘要预算三个字段。
///
/// v3 → v4（本轮）：模板/区域改名与删除，**不再断代重置**，改由
/// [`super::migrations`] 在事务内按键搬运旧值：
///
/// | 旧键 | 新键 |
/// |---|---|
/// | `templates.friend_confirm` | `templates.confirm_button` |
/// | `moderation.confirm_region` | `moderation.confirm_button_region` |
///
/// | 删除的键 | 原因 |
/// |---|---|
/// | `templates.paimon_menu_panel` | 派蒙菜单锚点随旧版 UI 判定一起移除 |
/// | `screen.paimon_menu_rect` | 同上 |
/// | `startup.templates.wonderland_confirm` | 千星确认改用共用确认按钮模板 |
/// | `startup.wonderland_confirm_region` | 区域改由 `moderation.confirm_button_region` 承担 |
///
/// 只有**删除或改名**字段才需要升版本：新增字段由 serde 默认值补齐，加载器
/// 明确允许「存储里缺字段」，因此加配置项不再触发断代重置。
///
/// 有迁移路径的旧版本（[`super::migrations::has_migration_path`]）直接迁移；
/// 没有迁移路径的更老版本（v1/v2 及更早）仍走断代重置并留 `.bak` 备份。
pub const CONFIG_SCHEMA_VERSION: i64 = 4;

/// 打开连接（含头初始化）的重试次数：两个进程并发首次创建同一新库文件时，
/// SQLite 头初始化阶段的锁竞争发生在 busy_timeout 生效之前，需要有限重试吸收。
const OPEN_CONNECTION_RETRIES: usize = 3;

/// 共享配置存储句柄（阶段 4 起由 HTTP 接口持有）。
pub type SharedConfigStore = Arc<Mutex<ConfigStore>>;

impl ConfigStore {
    /// 两个进程同时首次打开同一新库文件时，SQLite 头初始化阶段的锁竞争发生在
    /// `busy_timeout` 生效之前（连接尚未建立，busy handler 不存在），可能立即报
    /// database is locked；有限重试（间隔递增）吸收该竞争窗口。
    pub(super) fn open_connection_with_retries(
        database_path: &Path,
    ) -> Result<rusqlite::Connection> {
        let mut last_error = None;
        for attempt in 0..OPEN_CONNECTION_RETRIES {
            match Self::open_connection(database_path) {
                Ok(opened) => return Ok(opened),
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
        Err(last_error
            .unwrap_or_else(|| anyhow::anyhow!("打开配置数据库失败: {}", database_path.display())))
    }

    /// 打开连接并设置连接参数（WAL 等）；独立成函数供 open 的并发重试使用。
    pub(super) fn open_connection(database_path: &Path) -> Result<rusqlite::Connection> {
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
    pub(super) fn initialize_connection(connection: &rusqlite::Connection) -> Result<()> {
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
    pub(super) fn ensure_schema(connection: &mut rusqlite::Connection, path: &Path) -> Result<()> {
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
                    "配置数据库表结构不匹配（配置表缺失），请修复配置三表（不要删除含歌曲缓存的数据库）: {}",
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
            bail!(
                "配置数据库表结构不匹配，请修复配置三表（不要删除含歌曲缓存的数据库）: {}",
                path.display()
            );
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
                "配置数据库 config_meta 缺少初始化数据，请修复配置三表（不要删除含歌曲缓存的数据库）: {}",
                path.display()
            );
        };
        // 低于当前版本：有迁移路径就平滑迁移（保留用户设置），没有才断代重置。
        if schema_version < CONFIG_SCHEMA_VERSION {
            if migrations::has_migration_path(schema_version) {
                return Self::migrate_previous_schema(connection, path, schema_version);
            }
            log::warn!(
                "配置 schema 版本 v{schema_version} 早于最早可迁移版本，缺少字段映射表，只能断代重置为默认值（迁移路径覆盖 v{} → v{CONFIG_SCHEMA_VERSION}）",
                migrations::migration_start_version(),
            );
            return Self::reset_previous_schema(connection, path, schema_version);
        }
        if schema_version > CONFIG_SCHEMA_VERSION {
            bail!(
                "配置数据库 schema 版本高于当前程序（{schema_version} > {CONFIG_SCHEMA_VERSION}），请升级程序或修复配置三表（不要删除含歌曲缓存的数据库）: {}",
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
    pub(super) fn initialize_database(
        connection: &mut rusqlite::Connection,
        path: &Path,
    ) -> Result<()> {
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
    pub(super) fn initialize_database_transaction(
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
}

/// 当前毫秒时间戳（系统时钟不可用时回退 0）。
pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}
/// 从 `config_sections` 读取全部段；独立成函数供 [`ConfigStore`] 与迁移事务共用。
pub(super) fn read_sections_from(connection: &rusqlite::Connection) -> Result<Map<String, Value>> {
    let mut statement = connection.prepare("SELECT section, value_json FROM config_sections")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut sections = Map::new();
    for row in rows {
        let (section, text) = row?;
        let value: Value =
            serde_json::from_str(&text).with_context(|| format!("解析配置段 {section} 失败"))?;
        sections.insert(section, value);
    }
    Ok(sections)
}

/// 读取全部历史快照 `(revision, snapshot_json)`，按 revision 升序。
pub(super) fn read_snapshot_rows(connection: &rusqlite::Connection) -> Result<Vec<(i64, String)>> {
    let mut statement = connection
        .prepare("SELECT revision, snapshot_json FROM config_revisions ORDER BY revision")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}
