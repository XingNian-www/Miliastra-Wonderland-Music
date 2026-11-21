//! `ConfigStore` 的库级集成测试：真实临时 SQLite 文件，不 mock 存储层。
//!
//! 覆盖六类风险：
//!
//! - **结构校验**：缺表、部分表、列类型、NOT NULL、主键绑定、CHECK 放宽；
//! - **并发**：同一新库并发首次打开、两个连接同一基线、写失败整体回滚；
//! - **缓存共存**：断代重置与平滑迁移都不得动 `cached_*` / `request_state` / 回复表；
//! - **密钥协议**：脱敏展示、空值与掩码保留、清除标记；
//! - **内容迁移**：旧 `answer_prompt` 在打开时改写一次，重复打开不涨 revision；
//! - **升级路径**：v3 平滑迁移、v2 断代重置、失败回滚、历史快照同步。
//!
//! 命名沿用拆分前的单文件测试；`stored_*` 系列一律用**独立连接**读库，避免用被测
//! 代码自己的读取路径证明自己没错。

use std::collections::BTreeMap;
use std::fs;
use std::sync::{Arc, Barrier, Mutex};

use super::codec::get_path;
use super::*;
use crate::config::migrations::migrate_answer_prompt_text;
use crate::config::types::normalize_native_path_string;
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
    let root = std::env::temp_dir().join(format!("config-store-{name}-{}", uuid::Uuid::new_v4()));
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

/// Web 显示层：root 之下的绝对路径回落为相对，root 之外保留绝对；
/// 只处理 schema 声明的路径字段，普通字符串不猜成路径。
#[test]
fn normalize_path_fields_relativizes_under_root_and_preserves_outside_root() {
    let root = std::env::temp_dir().join(format!("config-normalize-root-{}", uuid::Uuid::new_v4()));
    let inside = root.join("deps/assets/idioms.txt");
    let outside = std::env::temp_dir().join("config-normalize-outside/game.exe");
    let outside_text = outside.to_string_lossy().into_owned();
    let mut sections = Map::new();
    sections.insert(
        "idiom_chain".to_string(),
        json!({"lexicon_path": inside.to_string_lossy().into_owned()}),
    );
    sections.insert(
        "startup".to_string(),
        json!({"exe_path": outside_text.clone()}),
    );
    sections.insert("ai".to_string(), json!({"model": "gpt-4o/测试"}));

    normalize_path_fields(&mut sections, &root, true);

    assert_eq!(
        get_path(&sections, "idiom_chain.lexicon_path").and_then(Value::as_str),
        Some("deps/assets/idioms.txt"),
        "root 下的绝对路径必须回落为相对路径"
    );
    assert_eq!(
        get_path(&sections, "startup.exe_path").and_then(Value::as_str),
        Some(outside_text.as_str()),
        "root 之外的绝对路径必须保留"
    );
    assert_eq!(
        get_path(&sections, "ai.model").and_then(Value::as_str),
        Some("gpt-4o/测试"),
        "非路径字段不能被改写"
    );
}

/// 相对路径只做分隔符规范化（Windows 上 `/`→`\`，其他平台原样）。
#[test]
fn normalize_path_fields_keeps_relative_values_native() {
    let root = std::env::temp_dir();
    let mut sections = Map::new();
    sections.insert(
        "ocr".to_string(),
        json!({"charset": "deps/assets/chars.txt"}),
    );
    normalize_path_fields(&mut sections, &root, true);
    let expected = normalize_native_path_string("deps/assets/chars.txt");
    assert_eq!(
        get_path(&sections, "ocr.charset").and_then(Value::as_str),
        Some(expected.as_str())
    );
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
                 VALUES (1, ?1, 1, 0)",
            rusqlite::params![CONFIG_SCHEMA_VERSION],
        )
        .unwrap();
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
fn reply_templates_preserve_order_partial_replacement_empty_override_and_cas() {
    let (root, database_path) = temp_database("reply-store");
    let bootstrap = test_bootstrap(&database_path);
    let mut first = ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap();
    let mut second = ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap();
    assert_eq!(first.reply_revision().unwrap(), 0);
    assert!(first.reply_templates().unwrap().is_empty());
    assert!(first.save_reply_templates(0, &BTreeMap::new()).is_err());

    let initial = BTreeMap::from([
        (
            "@游戏A".to_string(),
            vec![
                StoredReply {
                    key: "first".into(),
                    text: "line one".into(),
                },
                StoredReply {
                    key: "second".into(),
                    text: "line two".into(),
                },
            ],
        ),
        (
            "@游戏B".to_string(),
            vec![StoredReply {
                key: "keep".into(),
                text: "retained".into(),
            }],
        ),
    ]);
    assert_eq!(first.save_reply_templates(0, &initial).unwrap(), 1);
    let loaded = first.reply_templates().unwrap();
    assert_eq!(loaded["@游戏A"], initial["@游戏A"]);
    assert_eq!(loaded["@游戏B"], initial["@游戏B"]);

    let updates = BTreeMap::from([
        (
            "@游戏A".to_string(),
            vec![StoredReply {
                key: "third".into(),
                text: "replacement".into(),
            }],
        ),
        ("@游戏B".to_string(), Vec::new()),
    ]);
    assert_eq!(first.save_reply_templates(1, &updates).unwrap(), 2);
    let saved = first.reply_templates().unwrap();
    assert_eq!(saved["@游戏A"], updates["@游戏A"]);
    assert!(saved["@游戏B"].is_empty());
    assert_eq!(first.reply_revision().unwrap(), 2);

    let stale = BTreeMap::from([("@游戏A".to_string(), Vec::new())]);
    assert!(second.save_reply_templates(1, &stale).is_err());
    assert_eq!(second.reply_revision().unwrap(), 2);
    assert_eq!(
        second.reply_templates().unwrap(),
        first.reply_templates().unwrap()
    );
    drop(second);
    drop(first);
    let reopened = ConfigStore::open(&database_path, &root, bootstrap).unwrap();
    assert_eq!(reopened.reply_revision().unwrap(), 2);
    let reopened_rows = reopened.reply_templates().unwrap();
    assert_eq!(reopened_rows["@游戏A"], updates["@游戏A"]);
    assert!(reopened_rows["@游戏B"].is_empty());
    cleanup(&root);
}

#[test]
fn previous_schema_resets_only_configuration_and_preserves_cache() {
    let (root, database_path) = temp_database("schema-reset-cache");
    let bootstrap = test_bootstrap(&database_path);
    let mut store = ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap();
    let reply_data = BTreeMap::from([(
        "@preserved".to_string(),
        vec![StoredReply {
            key: "answer".into(),
            text: "survives reset".into(),
        }],
    )]);
    assert_eq!(store.save_reply_templates(0, &reply_data).unwrap(), 1);
    let mut sections = full_sections(&store);
    sections["queue"]["max_size"] = json!(20);
    sections["ai"]["api_key"] = json!("old-secret");
    store.save(1, sections).unwrap();
    let audio_path = root.join("cached-song.audio");
    fs::write(&audio_path, b"cached audio bytes").unwrap();
    store.connection.execute_batch(
            "CREATE TABLE cached_tracks (hash TEXT PRIMARY KEY, title TEXT);
             CREATE TABLE cached_lyrics (hash TEXT PRIMARY KEY REFERENCES cached_tracks(hash) ON DELETE CASCADE, lyrics_json BLOB);
             CREATE TABLE request_state (id INTEGER PRIMARY KEY, snapshot_json TEXT);
             INSERT INTO cached_tracks VALUES ('song', 'cached title');
             INSERT INTO cached_lyrics VALUES ('song', X'010203');
             INSERT INTO request_state VALUES (1, 'request snapshot');
             PRAGMA user_version = 2;
             UPDATE config_meta SET schema_version = 1;
             UPDATE config_sections SET value_json = 'invalid old JSON';
             UPDATE config_revisions SET snapshot_json = 'invalid old snapshot';"
        ).unwrap();
    drop(store);

    let mut store = ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap();
    assert_eq!(store.reply_revision().unwrap(), 1);
    assert_eq!(store.reply_templates().unwrap(), reply_data);
    // 按真实持久化 JSON 往返比较；f32 转 JSON 数字再解析可能有末位舍入差异。
    let expected: Value =
        serde_json::from_str(&serde_json::to_string(&AppConfig::default().to_db_value()).unwrap())
            .unwrap();
    assert_eq!(Value::Object(store.read_all_sections().unwrap()), expected);
    assert_eq!(store.current_revision().unwrap(), 1);
    assert_eq!(store.revisions().unwrap().len(), 1);
    assert_eq!(latest_snapshot(&database_path), expected);
    store.load_full().unwrap();
    let cache: (String, Vec<u8>, String, i64) = store.connection.query_row(
            "SELECT title, lyrics_json, snapshot_json, (SELECT user_version FROM pragma_user_version)
             FROM cached_tracks JOIN cached_lyrics USING(hash) CROSS JOIN request_state", [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        ).unwrap();
    assert_eq!(
        cache,
        (
            "cached title".into(),
            vec![1, 2, 3],
            "request snapshot".into(),
            2
        )
    );
    assert_eq!(fs::read(&audio_path).unwrap(), b"cached audio bytes");
    assert!(matches!(
        store.rollback(2, 1),
        Err(ConfigSaveError::RevisionNotFound(2))
    ));

    // 断代之后同版本保存、重开不能再次重置。
    let mut sections = full_sections(&store);
    sections["queue"]["max_size"] = json!(23);
    store.save(1, sections).unwrap();
    drop(store);
    let store = ConfigStore::open(&database_path, &root, bootstrap).unwrap();
    assert_eq!(store.load_full().unwrap().queue.max_size, 23);
    assert_eq!(store.current_revision().unwrap(), 2);
    drop(store);
    cleanup(&root);
}

#[test]
fn v6_2_0_database_upgrades_by_resetting_config_and_keeping_cache() {
    // v6.2.0 的 CONFIG_SCHEMA_VERSION 是 1，7.0.0 是 2：跨版本按既定策略断代重置
    // 配置三表，但歌曲缓存、命令回复与请求快照必须完好，且升级前要留下备份。
    // 这条测试直接模拟"从最新正式发布版升级"，避免以后改版本号时误伤缓存。
    let (root, database_path) = temp_database("v620-upgrade");
    let bootstrap = test_bootstrap(&database_path);
    let mut store = ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap();

    let reply_data = BTreeMap::from([(
        "@升级保留".to_string(),
        vec![StoredReply {
            key: "answer".into(),
            text: "keep me".into(),
        }],
    )]);
    assert_eq!(store.save_reply_templates(0, &reply_data).unwrap(), 1);
    let audio_path = root.join("v620-cached.audio");
    fs::write(&audio_path, b"audio bytes").unwrap();
    store
        .connection
        .execute_batch(
            "CREATE TABLE cached_tracks (hash TEXT PRIMARY KEY, title TEXT);
             INSERT INTO cached_tracks VALUES ('v620', 'cached v620');
             PRAGMA user_version = 1;
             UPDATE config_meta SET schema_version = 1;",
        )
        .unwrap();
    drop(store);

    let mut store = ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap();

    // 配置回到默认值（断代），版本号进到当前值。
    let expected: Value =
        serde_json::from_str(&serde_json::to_string(&AppConfig::default().to_db_value()).unwrap())
            .unwrap();
    assert_eq!(Value::Object(store.read_all_sections().unwrap()), expected);
    assert_eq!(store.current_revision().unwrap(), 1);
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT schema_version FROM config_meta WHERE id = 1",
                [],
                |row| { row.get::<_, i64>(0) }
            )
            .unwrap(),
        CONFIG_SCHEMA_VERSION
    );

    // 缓存与回复模板不属于配置三表，必须跨版本保留。
    let cached: String = store
        .connection
        .query_row(
            "SELECT title FROM cached_tracks WHERE hash = 'v620'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(cached, "cached v620");
    assert_eq!(store.reply_templates().unwrap(), reply_data);
    assert_eq!(fs::read(&audio_path).unwrap(), b"audio bytes");

    // 升级后能正常加载并再次保存。
    store.load_full().unwrap().validate().unwrap();
    drop(store);

    // 断代前必须留下整库备份，且备份里还能读到升级前的配置与缓存。
    // 备份写在库文件同目录（deps/data），不是可执行根目录。
    let backups = backup_files(database_path.parent().unwrap());
    assert_eq!(backups.len(), 1, "升级应恰好产生一个备份: {backups:?}");
    let backup = rusqlite::Connection::open(&backups[0]).unwrap();
    let backed_up_version: i64 = backup
        .query_row(
            "SELECT schema_version FROM config_meta WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(backed_up_version, 1, "备份应保留升级前的 schema 版本");
    let backed_up_cache: String = backup
        .query_row(
            "SELECT title FROM cached_tracks WHERE hash = 'v620'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(backed_up_cache, "cached v620");

    cleanup(&root);
}

/// 临时目录里的 .bak 文件列表（按名字排序）。
fn backup_files(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "bak"))
        .collect();
    found.sort();
    found
}

#[test]
fn previous_schema_reset_failure_rolls_back_all_configuration_tables() {
    let (root, database_path) = temp_database("schema-reset-failure");
    let bootstrap = test_bootstrap(&database_path);
    let mut store = ConfigStore::open(&database_path, &root, bootstrap).unwrap();
    let before = store.read_all_sections().unwrap();
    // 引用旧 revision 的外部表阻止 DROP；失败必须回滚已执行的 DDL。
    store.connection.execute_batch(
            "UPDATE config_meta SET schema_version = 1;
             CREATE TABLE retained_reference (revision INTEGER REFERENCES config_revisions(revision));
             INSERT INTO retained_reference VALUES (1);"
        ).unwrap();
    assert!(ConfigStore::ensure_schema(&mut store.connection, &database_path).is_err());
    assert_eq!(store.read_all_sections().unwrap(), before);
    assert_eq!(store.revisions().unwrap().len(), 1);
    let version: i64 = store
        .connection
        .query_row("SELECT schema_version FROM config_meta", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(version, 1);
    drop(store);
    cleanup(&root);
}

#[test]
fn omitted_optional_song_source_survives_strict_load_save_and_reopen() {
    let (root, database_path) = temp_database("optional-song-source");
    let bootstrap = test_bootstrap(&database_path);
    let store = ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap();
    // 当前 schema 已有的权限段只有 overrides；新可选字段没有落库。
    //
    // 样例 id 用**真实存在的平台条目**，不要用已废弃的 `@点歌`：这条测试
    // 要验证的是「可选字段 default_song_source 未落库时能严格 load/save/reopen
    // 且不触发迁移」，与某个具体命令无关。用废弃 id 会让它顺带依赖 sanitize 行为。
    let access = json!({"overrides": {"@QQ点歌": {"enabled": false, "minimum_role": "admin"}}});
    store
        .connection
        .execute(
            "UPDATE config_sections SET value_json = ?1 WHERE section = 'command_access'",
            rusqlite::params![serde_json::to_string(&access).unwrap()],
        )
        .unwrap();
    drop(store);

    let mut store = ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap();
    let loaded = store.load_full().unwrap();
    assert_eq!(loaded.command_access.default_song_source, None);
    assert!(!loaded.command_access.overrides["@QQ点歌"].enabled);
    assert_eq!(store.current_revision().unwrap(), 1, "打开旧段不能触发迁移");
    assert_eq!(stored_section(&database_path, "command_access"), access);
    let sections = Map::from_iter([("command_access".into(), access.clone())]);
    assert!(store.validate_candidate(&sections).unwrap().is_empty());
    assert_eq!(store.save(1, sections).unwrap().revision, 2);
    assert_eq!(stored_section(&database_path, "command_access"), access);
    assert_eq!(latest_snapshot(&database_path)["command_access"], access);
    drop(store);

    let mut store = ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap();
    assert_eq!(
        store
            .load_full()
            .unwrap()
            .command_access
            .default_song_source,
        None
    );
    assert_eq!(store.current_value().unwrap()["command_access"], access);
    let mut with_source = access.clone();
    with_source["default_song_source"] = json!("netease");
    let sections = Map::from_iter([("command_access".into(), with_source.clone())]);
    assert_eq!(store.save(2, sections).unwrap().revision, 3);
    drop(store);

    let mut store = ConfigStore::open(&database_path, &root, bootstrap).unwrap();
    assert_eq!(
        store
            .load_full()
            .unwrap()
            .command_access
            .default_song_source
            .as_deref(),
        Some("netease")
    );
    assert_eq!(
        stored_section(&database_path, "command_access"),
        with_source
    );
    assert_eq!(
        latest_snapshot(&database_path)["command_access"],
        with_source
    );
    // 显式 null = 未设置：schema 声明 nullable，Web 表单空值就提交 null，
    // 必须与字段缺失等价（不再被「字段已不存在」拒绝）。
    let mut with_null = access.clone();
    with_null["default_song_source"] = Value::Null;
    let sections = Map::from_iter([("command_access".into(), with_null.clone())]);
    assert!(
        store.validate_candidate(&sections).unwrap().is_empty(),
        "显式 null 应通过预检"
    );
    assert_eq!(store.save(3, sections).unwrap().revision, 4);
    assert_eq!(stored_section(&database_path, "command_access"), with_null);
    assert_eq!(latest_snapshot(&database_path)["command_access"], with_null);
    drop(store);

    let mut store =
        ConfigStore::open(&database_path, &root, test_bootstrap(&database_path)).unwrap();
    assert_eq!(
        store
            .load_full()
            .unwrap()
            .command_access
            .default_song_source,
        None,
        "显式 null 重新加载后仍是未设置"
    );
    assert_eq!(store.current_value().unwrap()["command_access"], with_null);

    // 非法平台值仍然报中文错误，且不写库、不推进 revision。
    for invalid in [
        json!("unknown-platform"),
        json!(""),
        json!("qq_music"),
        json!("QQ音乐"),
    ] {
        let mut candidate = access.clone();
        candidate["default_song_source"] = invalid.clone();
        let sections = Map::from_iter([("command_access".into(), candidate)]);
        let errors = store.validate_candidate(&sections).unwrap();
        assert!(!errors.is_empty(), "非法值 {invalid} 应被拒绝");
        let message = errors[0].message.clone();
        assert!(
            message.contains("默认点歌平台") && message.contains("qqmusic"),
            "错误文案应可理解并列出可选平台，实际: {message}"
        );
        assert!(store.save(4, sections).is_err());
        assert_eq!(store.current_revision().unwrap(), 4);
        assert_eq!(stored_section(&database_path, "command_access"), with_null);
        assert_eq!(latest_snapshot(&database_path)["command_access"], with_null);
    }
    drop(store);
    cleanup(&root);
}

/// A1 集成回归：老配置库里还留着 `@点歌` override 时，
/// `load_full()` 必须**成功**并在内存里把这条死配置丢掉。
///
/// 这是「老用户不会因为一条早已不生效的条目启动失败」的真正防线：
/// 只测 sanitize 函数本身不够，要覆盖「读库 → 校验」这条真实链路。
/// 并且库里的原始值**保持不动**（不回写），回滚旧版本仍能读到。
#[test]
fn deprecated_song_override_in_an_old_database_loads_and_is_dropped() {
    let (root, database_path) = temp_database("deprecated-song-override");
    let bootstrap = test_bootstrap(&database_path);
    let store = ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap();
    let legacy = json!({"overrides": {"@点歌": {"enabled": false, "minimum_role": "admin"}}});
    store
        .connection
        .execute(
            "UPDATE config_sections SET value_json = ?1 WHERE section = 'command_access'",
            rusqlite::params![serde_json::to_string(&legacy).unwrap()],
        )
        .unwrap();
    drop(store);

    let mut store = ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap();
    let loaded = store
        .load_full()
        .expect("老库里的 @点歌 override 不能让加载失败");
    assert!(
        !loaded.command_access.overrides.contains_key("@点歌"),
        "废弃条目必须在内存里被丢弃"
    );
    // 丢弃只发生在内存，库里的原始值不动（回滚旧版本仍读得到）。
    assert_eq!(stored_section(&database_path, "command_access"), legacy);
    assert_eq!(store.current_revision().unwrap(), 1, "打开旧段不能触发迁移");

    // 保存预检同口径：同一份老 sections 预检必须通过（否则用户点什么保存都报错）。
    let sections = Map::from_iter([("command_access".into(), legacy.clone())]);
    assert!(
        store.validate_candidate(&sections).unwrap().is_empty(),
        "预检也必须先 sanitize 再 validate：{:?}",
        store.validate_candidate(&sections).unwrap()
    );
    drop(store);
    cleanup(&root);
}

#[test]
fn schema_tolerates_added_fields_but_rejects_removed_ones() {
    let (root, database_path) = temp_database("strict-current");
    let bootstrap = test_bootstrap(&database_path);
    let mut store = ConfigStore::open(&database_path, &root, bootstrap).unwrap();
    let original = store.read_all_sections().unwrap();
    let mut missing_section = original.clone();
    missing_section.remove("state");
    // 缺整个段同样按「新增」处理：默认值补齐，不报错。
    assert!(
        store.build_config(&missing_section).is_ok(),
        "缺段属于新增，应补默认值而不是报错"
    );
    let mut legacy = original.clone();
    legacy["song_review"]["provider"] = json!({"api_key":"old"});
    assert!(store.build_config(&legacy).is_err());
    // 缺字段不再报错：那是「程序新增了配置项」，默认值补齐即可。
    let mut incomplete = original.clone();
    let removed = incomplete["timing"]["playback"]
        .as_object_mut()
        .unwrap()
        .remove("lyrics_lead_seconds")
        .expect("测试字段必须存在");
    store
        .connection
        .execute(
            "UPDATE config_sections SET value_json = ?1 WHERE section = 'timing'",
            rusqlite::params![serde_json::to_string(&incomplete["timing"]).unwrap()],
        )
        .unwrap();
    let loaded = store.load_full().expect("缺字段必须按默认值补齐而不是报错");
    let _ = removed;
    assert_eq!(
        loaded.timing.playback.lyrics_lead_seconds,
        AppConfig::default().timing.playback.lyrics_lead_seconds,
        "补齐的应当是默认值"
    );
    // 空候选现在等于「全部用默认值」，不再是错误；这里只校验读路径。
    // 读回原始存储值仍然是缺字段的形态，不做隐式回写。
    assert_eq!(store.read_all_sections().unwrap(), incomplete);
    drop(store);
    cleanup(&root);
}

#[test]
fn rollback_rejects_incomplete_snapshot_without_merging_current_fields() {
    let (root, database_path) = temp_database("strict-rollback");
    let bootstrap = test_bootstrap(&database_path);
    let mut store = ConfigStore::open(&database_path, &root, bootstrap).unwrap();
    let before = store.read_all_sections().unwrap();
    let mut snapshot = before.clone();
    snapshot.remove("timing");
    store
        .connection
        .execute(
            "UPDATE config_revisions SET snapshot_json = ?1 WHERE revision = 1",
            rusqlite::params![serde_json::to_string(&snapshot).unwrap()],
        )
        .unwrap();
    assert!(store.rollback(1, 1).is_err());
    assert_eq!(store.current_revision().unwrap(), 1);
    assert_eq!(store.read_all_sections().unwrap(), before);
    drop(store);
    cleanup(&root);
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
    sections["ai"]["extra_body"] = json!({"custom": {"nested": [1, {"flag": true}]}});
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
        reopened.current_value().unwrap()["ai"]["extra_body"],
        sections["ai"]["extra_body"]
    );
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

/// 反向门禁：没有 {{bot_history}} 的模板即使含旧 hall/history 片段也
/// 一律不改写（避免覆盖用户自定义）。
#[test]
fn answer_prompt_without_bot_history_is_never_rewritten() {
    let custom = "对话历史通过独立消息提供。\
大厅共享记忆（所有人的发言；你自己说过的话只经机器人历史提供，这里不重复）：{{hall}}\
自定义结尾";
    assert_eq!(migrate_answer_prompt_text(custom), custom);
}

/// 旧 answer_prompt 含 {{bot_history}} 时在 open 阶段一次性迁移；自定义
/// 模板（不含该占位）不动。
#[test]
fn legacy_bot_history_answer_prompt_is_migrated_on_open() {
    let (root, database_path) = temp_database("answer-prompt-migrate");
    let bootstrap = test_bootstrap(&database_path);
    let mut store = ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap();
    let old_prompt = "关于当前玩家的事实：{{facts}}更早的对话摘要：{{summary}}\
对话历史通过独立消息提供。\
大厅近期机器人发言（仅供避免重复，不代表对当前玩家说过）：{{bot_history}}\
大厅共享记忆（所有人的发言；你自己说过的话只经机器人历史提供，这里不重复）：{{hall}}";
    let mut sections = full_sections(&store);
    sections["ai_chat"]["answer_prompt"] = json!(old_prompt);
    store.save(1, sections).unwrap();
    drop(store);

    let store = ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap();
    let prompt = store.load_full().unwrap().ai_chat.answer_prompt;
    assert!(!prompt.contains("{{bot_history}}"), "{prompt}");
    assert!(!prompt.contains("大厅近期机器人发言"), "{prompt}");
    assert!(prompt.contains("其他玩家的发言"), "{prompt}");
    assert!(
        prompt.contains("个人问答历史默认作为独立消息提供"),
        "{prompt}"
    );
    assert_eq!(store.current_revision().unwrap(), 3, "迁移应写回一版");
    drop(store);

    // 再次打开不应重复迁移/继续涨 revision。
    let store = ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap();
    assert_eq!(store.current_revision().unwrap(), 3);
    drop(store);

    // 自定义模板（不含 bot_history）保持原样。
    let mut store = ConfigStore::open(&database_path, &root, bootstrap).unwrap();
    let mut sections = full_sections(&store);
    sections["ai_chat"]["answer_prompt"] = json!("自定义 {{facts}} 与 {{hall}}");
    store.save(3, sections).unwrap();
    drop(store);
    let store = ConfigStore::open(&database_path, &root, test_bootstrap(&database_path)).unwrap();
    assert_eq!(
        store.load_full().unwrap().ai_chat.answer_prompt,
        "自定义 {{facts}} 与 {{hall}}"
    );
    drop(store);
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

    // 手工把 schema 版本改成高于当前程序的值，模拟"降级运行"：
    // 低于当前版本的配置会被断代重置，高于当前版本必须明确失败，
    // 否则旧程序会把新版本配置静默覆盖成默认值。
    let connection = rusqlite::Connection::open(&database_path).unwrap();
    connection
        .execute(
            "UPDATE config_meta SET schema_version = 99 WHERE id = 1",
            [],
        )
        .unwrap();
    drop(connection);

    let error = ConfigStore::open(&database_path, &root, bootstrap)
        .expect_err("高于当前程序的 schema 版本必须明确失败");
    assert!(
        error.to_string().contains("高于当前程序"),
        "错误消息必须说明版本高于当前程序: {error}"
    );
    assert!(
        error.to_string().contains("不要删除"),
        "错误消息必须提示保留缓存数据库: {error}"
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
        error.to_string().contains("不要删除"),
        "错误消息必须提示保留缓存数据库: {error}"
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
                 VALUES (1, ?1, 1, 0)",
            rusqlite::params![CONFIG_SCHEMA_VERSION],
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
        error.to_string().contains("不要删除"),
        "错误消息必须提示保留缓存数据库: {error}"
    );

    // 绝不初始化：失败后其余配置表不得被创建。
    let connection = rusqlite::Connection::open(&database_path).unwrap();
    let sections_exists: bool = connection
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'config_sections'")
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
        error.to_string().contains("不要删除"),
        "错误消息必须提示保留缓存数据库: {error}"
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
        error.to_string().contains("不要删除"),
        "错误消息必须提示保留缓存数据库: {error}"
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
        error.to_string().contains("不要删除"),
        "错误消息必须提示保留缓存数据库: {error}"
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
        error.to_string().contains("不要删除"),
        "错误消息必须提示保留缓存数据库: {error}"
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
            error.to_string().contains("不要删除"),
            "错误消息必须提示保留缓存数据库: {error}"
        );
        cleanup(&root);
    }
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
        error.to_string().contains("不要删除"),
        "错误消息必须提示保留缓存数据库: {error}"
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
    let handle = std::thread::spawn(move || ConfigStore::ensure_schema(&mut connection_b, &path));
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
    let store = ConfigStore::open(&database_path, &root, test_bootstrap(&database_path)).unwrap();
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
    let root = std::env::temp_dir().join(format!("config-store-pipeline-{}", uuid::Uuid::new_v4()));
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
    let root = std::env::temp_dir().join(format!("config-store-minimal-{}", uuid::Uuid::new_v4()));
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
#[test]
fn current_schema_visual_profile_round_trip_preserves_saved_geometry() {
    let (root, database_path) = temp_database("legacy-visual-profile");
    let bootstrap = test_bootstrap(&database_path);
    drop(ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap());
    let mut ocr = stored_section(&database_path, "ocr");
    ocr["marker_search_width"] = json!(60);
    ocr["text_left_gap"] = serde_json::json!(17);
    let mut templates = stored_section(&database_path, "templates");
    templates["panel_downsample_factors"] = json!({});
    templates["marker_threshold"] = serde_json::json!(0.83);
    let connection = rusqlite::Connection::open(&database_path).unwrap();
    for (key, value) in [("ocr", ocr), ("templates", templates)] {
        connection
            .execute(
                "UPDATE config_sections SET value_json=?1 WHERE section=?2",
                rusqlite::params![serde_json::to_string(&value).unwrap(), key],
            )
            .unwrap();
    }
    drop(connection);
    let mut store = ConfigStore::open(&database_path, &root, bootstrap).unwrap();
    let config = store.load_full().unwrap();
    assert_eq!(config.ocr.marker_search_width, 60);
    assert_eq!(config.ocr.text_left_gap, 17);
    assert!(config.templates.panel_downsample_factors.is_empty());
    let raw = store.current_value().unwrap();
    let mut profile = crate::config::visual::VisualProfile::export(&raw).unwrap();
    assert_eq!(profile.thresholds["builtin"], serde_json::json!(0.83));
    profile
        .chat_geometry
        .insert("marker_search_width".into(), serde_json::json!(72));
    let sections = profile.candidate(&raw).unwrap();
    let revision = store.current_revision().unwrap();
    store.save(revision, sections).unwrap();
    assert_eq!(store.load_full().unwrap().ocr.marker_search_width, 72);
    assert_eq!(store.load_full().unwrap().ocr.text_left_gap, 17);
    drop(store);
    cleanup(&root);
}

/// 构造一份 v3 视图的 sections：把本轮改名/删除的字段还原成 v3 的键。
/// 用来喂给真实的「打开旧库 → 迁移」路径，而不是只测纯函数。
fn v3_sections_from_current(store: &ConfigStore) -> Map<String, Value> {
    let mut sections = store.read_all_sections().unwrap();
    {
        let templates = sections["templates"].as_object_mut().unwrap();
        let confirm = templates.remove("confirm_button").unwrap();
        templates.insert("friend_confirm".into(), confirm);
        templates.insert(
            "paimon_menu_panel".into(),
            json!("deps/assets/ui-paimon-menu-panel.png"),
        );
    }
    {
        let moderation = sections["moderation"].as_object_mut().unwrap();
        let region = moderation.remove("confirm_button_region").unwrap();
        moderation.insert("confirm_region".into(), region);
    }
    sections["screen"].as_object_mut().unwrap().insert(
        "paimon_menu_rect".into(),
        json!({"x": 152, "y": 358, "width": 92, "height": 92}),
    );
    let startup = sections["startup"].as_object_mut().unwrap();
    startup.insert(
        "wonderland_confirm_region".into(),
        json!({"x": 900, "y": 650, "width": 500, "height": 250}),
    );
    startup["templates"].as_object_mut().unwrap().insert(
        "wonderland_confirm".into(),
        json!("deps/assets/startup-wonderland-confirm.png"),
    );
    // 真实 v3 库还没有本轮新增字段（例如 moderation.confirm_message_region
    // 没有 serde 默认值）；迁移必须补齐，否则重开时 load_full 会 missing field。
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
    sections
}

/// 把 sections 与指定 schema 版本写回库（模拟上一版程序留下的库）。
fn write_sections_as_version(database_path: &Path, version: i64, sections: &Map<String, Value>) {
    let connection = rusqlite::Connection::open(database_path).unwrap();
    for (section, value) in sections {
        connection
            .execute(
                "UPDATE config_sections SET value_json = ?1 WHERE section = ?2",
                rusqlite::params![serde_json::to_string(value).unwrap(), section],
            )
            .unwrap();
    }
    connection
        .execute(
            "UPDATE config_meta SET schema_version = ?1 WHERE id = 1",
            rusqlite::params![version],
        )
        .unwrap();
}

/// 读取 config_meta 的 schema 版本（独立连接）。
fn stored_schema_version(database_path: &Path) -> i64 {
    let connection = rusqlite::Connection::open(database_path).unwrap();
    connection
        .query_row(
            "SELECT schema_version FROM config_meta WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn v3_database_migrates_fields_in_place_and_keeps_user_settings() {
    let (root, database_path) = temp_database("v3-migrate");
    let bootstrap = test_bootstrap(&database_path);
    let mut store = ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap();
    // 先写入用户设置，再降级成 v3 视图，最后重开触发迁移。
    let reply_data = BTreeMap::from([(
        "@迁移保留".to_string(),
        vec![StoredReply {
            key: "answer".into(),
            text: "survives migration".into(),
        }],
    )]);
    assert_eq!(store.save_reply_templates(0, &reply_data).unwrap(), 1);
    let mut sections = full_sections(&store);
    sections["queue"]["max_size"] = json!(23);
    sections["ai"]["api_key"] = json!("v3-secret");
    store.save(1, sections).unwrap();
    let mut v3 = v3_sections_from_current(&store);
    v3["queue"]["max_size"] = json!(23);
    v3["ai"]["api_key"] = json!("v3-secret");
    // 用户自定义的确认按钮模板必须跨迁移保留（改名后仍是同一个值）。
    v3["templates"]["friend_confirm"] = json!("custom/我的确认按钮.png");
    let revision_before = store.current_revision().unwrap();
    drop(store);
    write_sections_as_version(&database_path, 3, &v3);

    let mut store = ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap();

    // 版本升到 4，配置不再被重置（revision 不变、用户设置还在）。
    assert_eq!(stored_schema_version(&database_path), CONFIG_SCHEMA_VERSION);
    assert_eq!(stored_schema_version(&database_path), 4);
    assert_eq!(store.current_revision().unwrap(), revision_before);
    assert_eq!(
        stored_section(&database_path, "queue")["max_size"],
        json!(23)
    );
    assert_eq!(
        stored_section(&database_path, "ai")["api_key"],
        json!("v3-secret")
    );
    // 旧键消失、新键有值。
    let templates = stored_section(&database_path, "templates");
    assert_eq!(
        templates["confirm_button"],
        json!("custom/我的确认按钮.png"),
        "旧键的值必须搬到新键"
    );
    assert!(templates.get("friend_confirm").is_none());
    assert!(templates.get("paimon_menu_panel").is_none());
    let moderation = stored_section(&database_path, "moderation");
    assert!(moderation.get("confirm_button_region").is_some());
    assert!(moderation.get("confirm_region").is_none());
    let screen = stored_section(&database_path, "screen");
    assert!(screen.get("paimon_menu_rect").is_none());
    let startup = stored_section(&database_path, "startup");
    assert!(startup.get("wonderland_confirm_region").is_none());
    assert!(
        startup["templates"].get("wonderland_confirm").is_none(),
        "startup.templates.wonderland_confirm 必须被删掉"
    );
    // 迁移后能正常加载，且回复模板不受影响。
    let loaded = store.load_full().unwrap();
    assert_eq!(loaded.queue.max_size, 23);
    // load_full 会按 EXE 根目录解析相对路径，所以期望值是 root 下的绝对路径。
    assert_eq!(
        loaded.templates.confirm_button,
        root.join("custom/我的确认按钮.png"),
    );
    assert_eq!(store.reply_templates().unwrap(), reply_data);
    // 迁移前留下 .bak 备份。
    assert_eq!(backup_files(database_path.parent().unwrap()).len(), 1);
    cleanup(&root);
}

#[test]
fn migration_prefers_the_new_key_when_both_are_present() {
    let (root, database_path) = temp_database("v3-both-keys");
    let bootstrap = test_bootstrap(&database_path);
    let store = ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap();
    let mut v3 = v3_sections_from_current(&store);
    // 旧键与新键同时存在：新键必须胜出。
    v3["templates"]["confirm_button"] = json!("custom/新.png");
    v3["templates"]["friend_confirm"] = json!("custom/旧.png");
    drop(store);
    write_sections_as_version(&database_path, 3, &v3);

    let mut store = ConfigStore::open(&database_path, &root, bootstrap).unwrap();
    let templates = stored_section(&database_path, "templates");
    assert_eq!(
        templates["confirm_button"],
        json!("custom/新.png"),
        "两者都有时必须保留新键的值"
    );
    assert!(templates.get("friend_confirm").is_none(), "旧键仍要被删除");
    assert_eq!(
        store.load_full().unwrap().templates.confirm_button,
        root.join("custom/新.png"),
    );
    cleanup(&root);
}

#[test]
fn current_schema_database_is_not_migrated_again() {
    let (root, database_path) = temp_database("v4-no-remigrate");
    let bootstrap = test_bootstrap(&database_path);
    let mut store = ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap();
    let mut sections = full_sections(&store);
    sections["templates"]["confirm_button"] = json!("custom/当前.png");
    store.save(1, sections).unwrap();
    let revision = store.current_revision().unwrap();
    drop(store);

    let store = ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap();
    assert_eq!(stored_schema_version(&database_path), CONFIG_SCHEMA_VERSION);
    assert_eq!(
        store.current_revision().unwrap(),
        revision,
        "已是当前版本时不得触发迁移（revision 不能变）"
    );
    assert_eq!(
        stored_section(&database_path, "templates")["confirm_button"],
        json!("custom/当前.png")
    );
    assert!(
        backup_files(database_path.parent().unwrap()).is_empty(),
        "同版本打开不应产生备份"
    );
    drop(store);
    cleanup(&root);
}

#[test]
fn v2_database_without_a_migration_path_is_still_reset() {
    let (root, database_path) = temp_database("v2-reset");
    let bootstrap = test_bootstrap(&database_path);
    let mut store = ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap();
    let mut sections = full_sections(&store);
    sections["queue"]["max_size"] = json!(23);
    store.save(1, sections).unwrap();
    let mut v3 = v3_sections_from_current(&store);
    v3["queue"]["max_size"] = json!(23);
    drop(store);
    // v2 没有迁移路径：即使它的键看起来像 v3，也必须走断代重置。
    write_sections_as_version(&database_path, 2, &v3);

    let mut store = ConfigStore::open(&database_path, &root, bootstrap).unwrap();
    assert_eq!(stored_schema_version(&database_path), CONFIG_SCHEMA_VERSION);
    assert_eq!(
        store.current_revision().unwrap(),
        1,
        "断代重置回到 revision 1"
    );
    let expected: Value =
        serde_json::from_str(&serde_json::to_string(&AppConfig::default().to_db_value()).unwrap())
            .unwrap();
    assert_eq!(
        Value::Object(store.read_all_sections().unwrap()),
        expected,
        "无迁移路径的旧版本仍按默认值重置"
    );
    assert!(
        !store
            .load_full()
            .unwrap()
            .templates
            .confirm_button
            .as_os_str()
            .is_empty()
    );
    assert_eq!(backup_files(database_path.parent().unwrap()).len(), 1);
    cleanup(&root);
}

#[test]
fn failed_migration_rolls_back_and_keeps_the_original_configuration() {
    let (root, database_path) = temp_database("v3-migrate-failure");
    let bootstrap = test_bootstrap(&database_path);
    let store = ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap();
    let mut v3 = v3_sections_from_current(&store);
    // 损坏的段：迁移末尾的结构校验必然失败。
    v3.insert("queue".into(), json!("not-an-object"));
    let before = v3.clone();
    drop(store);
    write_sections_as_version(&database_path, 3, &v3);

    let error = ConfigStore::open(&database_path, &root, bootstrap.clone())
        .expect_err("损坏的配置必须让迁移失败而不是静默重置");
    let message = format!("{error:#}");
    assert!(
        message.contains("已回滚本次事务") && message.contains("原配置保持不变"),
        "错误信息应说明回滚与原配置未变: {message}"
    );
    // 原库必须保持迁移前状态：版本仍是 3、sections 一字未改。
    assert_eq!(stored_schema_version(&database_path), 3);
    assert_eq!(
        stored_section(&database_path, "queue"),
        json!("not-an-object")
    );
    assert_eq!(
        stored_section(&database_path, "templates"),
        before["templates"]
    );
    assert_eq!(stored_section(&database_path, "screen"), before["screen"]);
    // 备份仍然留下（迁移失败不删还原点）。
    assert_eq!(backup_files(database_path.parent().unwrap()).len(), 1);

    // 修好数据后重开能正常迁移，证明失败没有留下半写状态。
    let mut fixed = before.clone();
    fixed.insert(
        "queue".into(),
        AppConfig::default().to_db_value()["queue"].clone(),
    );
    write_sections_as_version(&database_path, 3, &fixed);
    let mut store = ConfigStore::open(&database_path, &root, bootstrap).unwrap();
    assert_eq!(stored_schema_version(&database_path), CONFIG_SCHEMA_VERSION);
    assert_eq!(
        stored_section(&database_path, "templates")["confirm_button"],
        fixed["templates"]["friend_confirm"]
    );
    store.load_full().unwrap();
    cleanup(&root);
}

#[test]
fn migration_also_rewrites_history_snapshots_so_rollback_still_works() {
    let (root, database_path) = temp_database("v3-migrate-snapshots");
    let bootstrap = test_bootstrap(&database_path);
    let mut store = ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap();
    // 造一版旧结构快照（revision 1 就是初始化快照）。
    let mut v3 = v3_sections_from_current(&store);
    v3["templates"]["friend_confirm"] = json!("custom/旧快照.png");
    let connection = rusqlite::Connection::open(&database_path).unwrap();
    connection
        .execute(
            "UPDATE config_revisions SET snapshot_json = ?1 WHERE revision = 1",
            rusqlite::params![serde_json::to_string(&Value::Object(v3.clone())).unwrap()],
        )
        .unwrap();
    drop(connection);
    store
        .connection
        .execute_batch("UPDATE config_meta SET schema_version = 3")
        .unwrap();
    drop(store);

    let mut store = ConfigStore::open(&database_path, &root, bootstrap).unwrap();
    // 迁移后，旧快照也必须能用当前结构解析（否则回滚到它必然报错）。
    let revision = store.current_revision().unwrap();
    let outcome = store.rollback(1, revision).unwrap();
    assert_eq!(outcome.revision, revision + 1);
    assert_eq!(
        store.load_full().unwrap().templates.confirm_button,
        root.join("custom/旧快照.png"),
        "回滚到迁移前的快照仍要保留确认按钮模板"
    );
    cleanup(&root);
}
