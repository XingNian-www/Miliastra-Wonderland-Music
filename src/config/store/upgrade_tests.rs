//! W5 迁移/隐私半（W5a-2）的回归测试。
//!
//! 覆盖四件事：
//!
//! - `.bak` 保留策略：迁移后每个库最多留最近 [`BACKUP_RETENTION`] 份；
//! - **合成 v3 夹具**：三份全部由本文件的代码按 v3 schema 生成，数据只用
//!   `示例玩家甲` / `uid-0001` / `token-placeholder` 这类占位符，**绝不拷贝真实库**；
//! - `config migrate --dry-run --json`：报告 rename / prune / fill 且不写库；
//! - 「库比程序新」：预演与真实打开都给出结构化错误，且不改库。
//!
//! # 夹具为什么不提交二进制
//!
//! 独立审查（`FINAL_REVIEW.md` §4.3-D2）确认 `scripts/privacy-filter.py` 的历史
//! 版本**不扫** `.sqlite3`：真实夹具里塞多少真实 UID/昵称，CI 都会报 0 命中，而
//! Git 历史一旦写入就不可回退。因此这里选择「测试代码生成」：
//!
//! - 生成函数 [`fixture_sections`] 只用 `AppConfig::default()` + 占位符；
//! - 生成物落在 `src/config/store/fixtures/`，但被同目录的 `.gitignore` 忽略，
//!   `git add -A` 收不到（配合隐私脚本新增的「数据型文件禁止入库」检查双保险）；
//! - `expected/v4_*.json` 是**文本黄金快照**，可以安全入库，用于发现迁移行为漂移。
//!
//! 需要重新生成时：`UPDATE_CONFIG_FIXTURES=1 cargo test --lib --no-default-features
//! config::store::upgrade::upgrade_tests`。生成结果只依赖当前代码与默认配置，
//! 不读取任何用户数据。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::{Map, Value, json};

use super::{BACKUP_RETENTION, CONFIG_SCHEMA_VERSION, ConfigStore};
use crate::config::migrations;
use crate::config::{AppConfig, BootstrapConfig, HttpConfig, LoggingConfig};

/// 三份合成夹具的基名。
const FIXTURE_NAMES: [&str; 3] = ["v3_minimal", "v3_customized", "v3_dirty"];

/// 生成夹具时写死的毫秒时间戳（2023-11-14T22:13:20Z），保证可复现。
const FIXTURE_TIMESTAMP_MS: i64 = 1_700_000_000_000;

fn test_bootstrap(database_path: &Path) -> BootstrapConfig {
    BootstrapConfig {
        database_path: database_path.to_path_buf(),
        http: HttpConfig::default(),
        logging: LoggingConfig::default(),
    }
}

fn temp_database(name: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("config-upgrade-{name}-{}", uuid::Uuid::new_v4()));
    let database_path = root.join("deps/data/playback.sqlite3");
    fs::create_dir_all(database_path.parent().unwrap()).unwrap();
    (root, database_path)
}

fn cleanup(root: &Path) {
    let _ = fs::remove_dir_all(root);
}

/// 临时目录里的 `.bak` 文件列表（按名字排序）。
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

/// 把当前库（v4）还原成 v3 视图：恢复旧键、补回已删除的旧字段、去掉本轮新增字段。
fn downgrade_to_v3(sections: &mut Map<String, Value>) {
    {
        let templates = sections["templates"].as_object_mut().unwrap();
        let confirm = templates
            .remove("confirm_button")
            .expect("默认配置必须有 templates.confirm_button");
        templates.insert("friend_confirm".into(), confirm);
        templates.insert(
            "paimon_menu_panel".into(),
            json!("deps/assets/example-paimon-menu-panel.png"),
        );
    }
    {
        let moderation = sections["moderation"].as_object_mut().unwrap();
        let region = moderation
            .remove("confirm_button_region")
            .expect("默认配置必须有 moderation.confirm_button_region");
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
        json!("deps/assets/example-startup-wonderland-confirm.png"),
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
}

/// 当前库的 v3 视图（用于把临时库降级成 v3）。
fn v3_view_of(store: &ConfigStore) -> Map<String, Value> {
    let mut sections = store.read_all_sections().unwrap();
    downgrade_to_v3(&mut sections);
    sections
}

/// 按 v3 schema 构造夹具的 sections：默认配置 + v3 旧键 + 明显占位符。
fn fixture_sections(name: &str) -> Map<String, Value> {
    let mut sections = AppConfig::default()
        .to_db_value()
        .as_object()
        .cloned()
        .expect("AppConfig::to_db_value 必须是对象");
    downgrade_to_v3(&mut sections);
    match name {
        // 最小库：只有默认值，没有用户自定义。
        "v3_minimal" => {
            sections["identity"] = json!({"mappings": []});
        }
        // 用户改过的库：自定义队列上限、密钥、确认按钮模板、身份映射与模板映射。
        "v3_customized" => {
            sections["queue"]["max_size"] = json!(23);
            sections["ai"]["api_key"] = json!("token-placeholder");
            sections["templates"]["friend_confirm"] =
                json!("deps/assets/example-confirm-button.png");
            sections["identity"] = json!({
                "mappings": [{
                    "nickname": "示例玩家甲",
                    "id": "00000000-0000-4000-8000-000000000001",
                    "role": "owner",
                    "note": "示例玩家甲",
                    "real_nickname": "示例玩家甲",
                    "remark_sourced": false,
                    "accounts": [{"game": "genshin", "uid": "uid-0001"}]
                }]
            });
            sections["custom_workflows"]["templates"] = json!({
                "示例占位模板": "deps/assets/example-placeholder.png"
            });
        }
        // 脏库：queue 里 2 条当前 schema 已不声明的键。迁移的 prune 阶段会把它们
        // 清掉；W5a-1 的 quarantine 表落地后，这 2 条应改写入 quarantine。
        "v3_dirty" => {
            sections["queue"]["legacy_placeholder_a"] = json!("dirty-placeholder-a");
            sections["queue"]["legacy_placeholder_b"] = json!("dirty-placeholder-b");
        }
        other => panic!("未知夹具: {other}"),
    }
    sections
}

/// 在内存里跑与 `ConfigStore::open` 相同的结构+内容迁移管线（用于生成黄金快照）。
fn migrate_in_memory(sections: &Map<String, Value>) -> Map<String, Value> {
    let mut migrated: BTreeMap<String, Value> = sections.clone().into_iter().collect();
    migrations::migrate_sections(3, &mut migrated).expect("v3 夹具必须能迁移");
    migrations::normalize_content(&mut migrated).expect("v3 夹具必须能跑内容迁移");
    migrated.into_iter().collect()
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/config/store/fixtures")
}

fn expected_path(dir: &Path, name: &str) -> PathBuf {
    let short = name.strip_prefix("v3_").unwrap_or(name);
    dir.join("expected").join(format!("v4_{short}.json"))
}

/// 按 v3 schema 写一份 SQLite 夹具：配置三表 + 一条缓存表数据（验证迁移不碰缓存）。
fn write_fixture_database(path: &Path, sections: &Map<String, Value>, name: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let staging = path.with_extension("sqlite3.tmp");
    let _ = fs::remove_file(&staging);
    let connection = rusqlite::Connection::open(&staging).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE config_meta (
                 id INTEGER PRIMARY KEY CHECK (id = 1),
                 schema_version INTEGER NOT NULL,
                 revision INTEGER NOT NULL,
                 updated_at_ms INTEGER NOT NULL
             );
             CREATE TABLE config_sections (
                 section TEXT PRIMARY KEY,
                 value_json TEXT NOT NULL,
                 revision INTEGER NOT NULL,
                 updated_at_ms INTEGER NOT NULL
             );
             CREATE TABLE config_revisions (
                 revision INTEGER PRIMARY KEY,
                 snapshot_json TEXT NOT NULL,
                 created_at_ms INTEGER NOT NULL
             );
             CREATE TABLE cached_songs (
                 song_id TEXT PRIMARY KEY,
                 title TEXT NOT NULL
             );",
        )
        .unwrap();
    let revision: i64 = if name == "v3_customized" { 2 } else { 1 };
    for (section, value) in sections {
        connection
            .execute(
                "INSERT INTO config_sections (section, value_json, revision, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    section,
                    serde_json::to_string(value).unwrap(),
                    revision,
                    FIXTURE_TIMESTAMP_MS
                ],
            )
            .unwrap();
    }
    // customized 库带一条更老的历史快照，验证迁移会同步改写全部快照。
    for snapshot_revision in 1..=revision {
        connection
            .execute(
                "INSERT INTO config_revisions (revision, snapshot_json, created_at_ms)
                 VALUES (?1, ?2, ?3)",
                rusqlite::params![
                    snapshot_revision,
                    serde_json::to_string(&Value::Object(sections.clone())).unwrap(),
                    FIXTURE_TIMESTAMP_MS
                ],
            )
            .unwrap();
    }
    connection
        .execute(
            "INSERT INTO config_meta (id, schema_version, revision, updated_at_ms)
             VALUES (1, 3, ?1, ?2)",
            rusqlite::params![revision, FIXTURE_TIMESTAMP_MS],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO cached_songs (song_id, title) VALUES ('song-placeholder-0001', '示例占位歌曲')",
            [],
        )
        .unwrap();
    drop(connection);
    // 原子替换：并发测试同时生成时不会读到半写文件。
    fs::rename(&staging, path).unwrap();
}

/// 首次运行（或 `UPDATE_CONFIG_FIXTURES=1`）时生成 3 份合成夹具与黄金快照。
fn ensure_fixtures() -> PathBuf {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = fixture_dir();
    fs::create_dir_all(dir.join("expected")).unwrap();
    let update = std::env::var_os("UPDATE_CONFIG_FIXTURES").is_some();
    for name in FIXTURE_NAMES {
        let path = dir.join(format!("{name}.sqlite3"));
        let expected = expected_path(&dir, name);
        if update || !path.is_file() || !expected.is_file() {
            let sections = fixture_sections(name);
            let migrated = migrate_in_memory(&sections);
            let golden = json!({
                "fixture": format!("{name}.sqlite3"),
                "synthetic": true,
                "generated_by": "src/config/store/upgrade_tests.rs（只含占位符，不读取任何真实配置库）",
                "migrated_schema_version": 4,
                "sections": Value::Object(migrated),
            });
            write_fixture_database(&path, &sections, name);
            let mut text = serde_json::to_string_pretty(&golden).unwrap();
            text.push('\n');
            fs::write(&expected, text).unwrap();
            eprintln!(
                "已生成合成 v3 夹具与黄金快照: {} / {}",
                path.display(),
                expected.display()
            );
        }
    }
    dir
}

fn expected_sections(dir: &Path, name: &str) -> Map<String, Value> {
    let path = expected_path(dir, name);
    let text = fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "缺少黄金快照 {}：{error}（可设 UPDATE_CONFIG_FIXTURES=1 重新生成）",
            path.display()
        )
    });
    let value: Value = serde_json::from_str(&text).unwrap();
    value["sections"]
        .as_object()
        .cloned()
        .unwrap_or_else(|| panic!("{} 缺少 sections 对象", path.display()))
}

fn copy_fixture_to_temp(dir: &Path, name: &str) -> (PathBuf, PathBuf) {
    let (root, database_path) = temp_database(&format!("fixture-{name}"));
    fs::copy(dir.join(format!("{name}.sqlite3")), &database_path).unwrap();
    (root, database_path)
}

/// 用独立连接读全部 sections（不用被测代码自己的读取路径）。
fn read_stored_sections(database_path: &Path) -> Map<String, Value> {
    let connection = rusqlite::Connection::open_with_flags(
        database_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let mut statement = connection
        .prepare("SELECT section, value_json FROM config_sections")
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap();
    let mut sections = Map::new();
    for row in rows {
        let (section, text) = row.unwrap();
        sections.insert(section, serde_json::from_str(&text).unwrap());
    }
    sections
}

fn read_cached_song_titles(database_path: &Path) -> Vec<(String, String)> {
    let connection = rusqlite::Connection::open_with_flags(
        database_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let mut statement = connection
        .prepare("SELECT song_id, title FROM cached_songs ORDER BY song_id")
        .unwrap();
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

/// `config_quarantine` 当前行数；表尚未落地（W5a-1 未合并）时返回 `None`。
///
/// 用「表在不在」做能力探测，让本测试在 W5a-1 合入前后都能通过：合入后断言
/// 2 条脏键真的进了 quarantine，未合入时退化为 TODO（见测试内说明）。
fn read_quarantine_count(database_path: &Path) -> Option<i64> {
    let connection = rusqlite::Connection::open_with_flags(
        database_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let exists = connection
        .prepare(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'config_quarantine'",
        )
        .unwrap()
        .exists([])
        .unwrap();
    if !exists {
        return None;
    }
    Some(
        connection
            .query_row("SELECT COUNT(*) FROM config_quarantine", [], |row| {
                row.get(0)
            })
            .unwrap(),
    )
}

fn set_file_modified(path: &Path, modified: SystemTime) {
    let file = fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_modified(modified).unwrap();
}

/// `.bak` 保留策略：先造 5 份历史备份，迁移后只留最新 3 份（含刚生成的那份）。
#[test]
fn backup_retention_keeps_three() {
    let (root, database_path) = temp_database("backup-retention");
    let bootstrap = test_bootstrap(&database_path);
    let store = ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap();
    let mut v3 = v3_view_of(&store);
    drop(store);
    write_sections_as_version(&database_path, 3, &v3);

    // 5 份历史备份：文件名符合 `<库名>.v<版本>.<时间戳>.bak`，mtime 依次递增。
    let now = SystemTime::now();
    let file_stem = database_path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let mut created = Vec::new();
    for index in 0..5u64 {
        let path =
            database_path.with_file_name(format!("{file_stem}.v3.{}.bak", 1_700_000_000 + index));
        fs::write(&path, format!("placeholder-backup-{index}")).unwrap();
        // index=4 最新、index=0 最旧；都早于「现在」生成的新备份。
        set_file_modified(&path, now - Duration::from_secs(5 - index));
        created.push(path);
    }

    let _store = ConfigStore::open(&database_path, &root, bootstrap).unwrap();

    let backups = backup_files(database_path.parent().unwrap());
    assert_eq!(
        backups.len(),
        BACKUP_RETENTION,
        "迁移后每个库只保留最近 {BACKUP_RETENTION} 份备份: {backups:?}"
    );
    for oldest in created.iter().take(3) {
        assert!(
            !oldest.exists(),
            "最旧的 3 份备份必须被清理: {}",
            oldest.display()
        );
    }
    for newest in created.iter().skip(3) {
        assert!(
            newest.exists(),
            "最新的 2 份历史备份必须保留: {}",
            newest.display()
        );
    }
    let fresh: Vec<&PathBuf> = backups
        .iter()
        .filter(|path| !created.contains(path))
        .collect();
    assert_eq!(fresh.len(), 1, "必须保留本次迁移新生成的备份: {backups:?}");
    assert_eq!(stored_schema_version(&database_path), CONFIG_SCHEMA_VERSION);
    cleanup(&root);
}

/// 三份**合成** v3 夹具都能迁移到 v4，且结果与 `expected/v4_*.json` 黄金快照一致。
#[test]
fn synthetic_v3_fixtures_migrate_to_expected_v4_snapshots() {
    let dir = ensure_fixtures();
    for name in ["v3_minimal", "v3_customized"] {
        let (root, database_path) = copy_fixture_to_temp(&dir, name);
        let expected = expected_sections(&dir, name);
        let cache_before = read_cached_song_titles(&database_path);
        let store =
            ConfigStore::open(&database_path, &root, test_bootstrap(&database_path)).unwrap();
        let loaded = store.load_full().unwrap();
        let revision = store.current_revision().unwrap();
        drop(store);

        assert_eq!(stored_schema_version(&database_path), CONFIG_SCHEMA_VERSION);
        assert_eq!(
            read_stored_sections(&database_path),
            expected,
            "{name}: 迁移结果与 expected/v4_*.json 不一致（设 UPDATE_CONFIG_FIXTURES=1 重新生成后人工审阅 diff）"
        );
        assert_eq!(
            read_cached_song_titles(&database_path),
            cache_before,
            "{name}: 迁移不得改动配置三表以外的缓存表"
        );
        assert!(revision >= 1);
        if name == "v3_customized" {
            assert_eq!(loaded.queue.max_size, 23, "用户自定义值必须保留");
            assert_eq!(loaded.ai.api_key, "token-placeholder");
            assert_eq!(loaded.identity.mappings.len(), 1);
            assert_eq!(loaded.identity.mappings[0].remark, "示例玩家甲");
            assert_eq!(loaded.identity.mappings[0].accounts[0].uid, "uid-0001");
        }
        cleanup(&root);
    }
}

/// 脏夹具：2 条当前 schema 不声明的键被清理，其余用户数据逐字节保留。
///
/// 断言分两层：
/// - 无论 W5a-1 是否合入：2 条脏键从 `config_sections` 消失，queue 其余字段与
///   identity / custom_workflows / 缓存表逐字节保留；
/// - W5a-1 的 `config_quarantine` 表已存在时：再断言 quarantine 恰好 2 行，
///   证明「先入 quarantine 再删」的契约生效（表缺失则打印 TODO，不把 W5a-1
///   当本文件的硬依赖）。
#[test]
fn v3_dirty_fixture_opens_with_two_quarantine_entries() {
    let dir = ensure_fixtures();
    let (root, database_path) = copy_fixture_to_temp(&dir, "v3_dirty");
    let before = read_stored_sections(&database_path);
    let cache_before = read_cached_song_titles(&database_path);
    let dirty_keys = ["legacy_placeholder_a", "legacy_placeholder_b"];
    for key in dirty_keys {
        assert!(
            before["queue"].get(key).is_some(),
            "脏夹具必须包含待清理条目 queue.{key}"
        );
    }

    let store = ConfigStore::open(&database_path, &root, test_bootstrap(&database_path)).unwrap();
    assert_eq!(stored_schema_version(&database_path), CONFIG_SCHEMA_VERSION);
    assert_eq!(store.current_revision().unwrap(), 1, "平滑迁移不重置配置");
    store.load_full().unwrap();
    drop(store);

    let after = read_stored_sections(&database_path);
    let mut expected_queue = before["queue"].as_object().unwrap().clone();
    for key in dirty_keys {
        assert!(
            !after["queue"].as_object().unwrap().contains_key(key),
            "脏条目 queue.{key} 必须被清理"
        );
        expected_queue.remove(key);
    }
    assert_eq!(
        after["queue"].as_object().unwrap().clone(),
        expected_queue,
        "clean 的 queue 字段必须逐字节保留"
    );
    assert_eq!(
        after["identity"], before["identity"],
        "identity 段不得被迁移改写"
    );
    assert_eq!(
        after["custom_workflows"], before["custom_workflows"],
        "custom_workflows 段不得被迁移改写"
    );
    assert_eq!(
        read_cached_song_titles(&database_path),
        cache_before,
        "缓存表不得被迁移改写"
    );
    // W5a-1 的 `config_quarantine` 表已合入时，2 条脏键必须真的可找回：
    // 断言行数 == 2；未合入时保持 TODO（本文件不把 W5a-1 当硬依赖）。
    match read_quarantine_count(&database_path) {
        Some(count) => assert_eq!(
            count, 2,
            "2 条被 prune 的脏键必须各写一条 config_quarantine（可找回）"
        ),
        None => eprintln!(
            "TODO(W5a-1)：config_quarantine 表尚未合入，暂以「2 条脏键被清理 + 其余数据不变」验收"
        ),
    }
    cleanup(&root);
}

/// dry-run：报告 rename / prune / fill，且不写库、不增删备份、不回写 config.yaml。
#[test]
fn dry_run_reports_renames_pruned_filled_without_writing() {
    let (root, database_path) = temp_database("dry-run");
    let bootstrap = test_bootstrap(&database_path);
    let store = ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap();
    let mut v3 = v3_view_of(&store);
    v3["queue"]["max_size"] = json!(23);
    v3["templates"]["friend_confirm"] = json!("deps/assets/example-confirm-button.png");
    v3["queue"]["legacy_unknown_placeholder"] = json!("dirty-placeholder");
    drop(store);
    write_sections_as_version(&database_path, 3, &v3);

    // 预置一份旧备份：dry-run 只能报告「将被清理」，不能真的删。
    let old_backup = database_path.with_file_name("playback.sqlite3.v3.1700000000.bak");
    fs::write(&old_backup, b"placeholder-backup").unwrap();

    let bytes_before = fs::read(&database_path).unwrap();
    let sections_before = read_stored_sections(&database_path);
    let backups_before = backup_files(database_path.parent().unwrap());

    let report = ConfigStore::dry_run_migration_report(&database_path).unwrap();
    assert_eq!(report["outcome"], "would_migrate", "{report}");
    assert_eq!(report["stored_version"], json!(3));
    assert_eq!(
        report["current_program_version"],
        json!(CONFIG_SCHEMA_VERSION)
    );
    let plan = report["plan"].as_object().unwrap();
    let renames = plan["renames"].as_array().unwrap();
    assert!(
        renames.iter().any(|item| item["section"] == "templates"
            && item["from"] == "friend_confirm"
            && item["to"] == "confirm_button"),
        "必须报告 templates.friend_confirm → confirm_button: {report}"
    );
    assert!(
        renames.iter().any(|item| item["section"] == "moderation"
            && item["from"] == "confirm_region"
            && item["to"] == "confirm_button_region"),
        "必须报告 moderation.confirm_region → confirm_button_region: {report}"
    );
    let pruned = plan["pruned"].as_array().unwrap();
    assert!(
        pruned
            .iter()
            .any(|item| item["section"] == "queue" && item["key"] == "legacy_unknown_placeholder"),
        "必须报告未知键会被 prune: {report}"
    );
    assert!(
        pruned
            .iter()
            .any(|item| item["section"] == "screen" && item["key"] == "paimon_menu_rect"),
        "必须报告已删除字段会被 prune: {report}"
    );
    let filled = plan["filled"].as_array().unwrap();
    assert!(
        filled
            .iter()
            .any(|item| item["section"] == "moderation" && item["key"] == "confirm_message_region"),
        "必须报告缺失字段会被默认值补齐: {report}"
    );
    assert_eq!(
        report["backup"]["retention"],
        json!(BACKUP_RETENTION),
        "备份报告必须带上保留上限"
    );
    assert!(
        report["backup"]["would_create"].is_string(),
        "会迁移时必须报告将要生成的备份名: {report}"
    );

    // 不写库：文件字节、sections、schema 版本都不变。
    assert_eq!(
        fs::read(&database_path).unwrap(),
        bytes_before,
        "dry-run 不得改动配置库文件"
    );
    assert_eq!(read_stored_sections(&database_path), sections_before);
    assert_eq!(stored_schema_version(&database_path), 3);
    assert_eq!(
        backup_files(database_path.parent().unwrap()),
        backups_before,
        "dry-run 不得创建或删除任何 .bak"
    );

    // config.yaml 入口：同样只读，且不会因为 access_token 为空而回写令牌。
    let config_path = root.join("config.yaml");
    fs::write(
        &config_path,
        "database_path: deps/data/playback.sqlite3\nhttp:\n  access_token: \"\"\n",
    )
    .unwrap();
    let config_bytes_before = fs::read(&config_path).unwrap();
    let report_from_config =
        ConfigStore::dry_run_migration_report_from_config(&config_path).unwrap();
    assert_eq!(report_from_config["outcome"], "would_migrate");
    assert_eq!(
        fs::read(&config_path).unwrap(),
        config_bytes_before,
        "dry-run 不得回写 config.yaml（令牌自动生成属于正式启动路径）"
    );
    cleanup(&root);
}

/// 库比程序新：预演给出结构化错误，真实打开同样拒绝且不改库。
#[test]
fn newer_database_yields_structured_error() {
    let (root, database_path) = temp_database("newer-database");
    let bootstrap = test_bootstrap(&database_path);
    drop(ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap());
    let connection = rusqlite::Connection::open(&database_path).unwrap();
    connection
        .execute(
            "UPDATE config_meta SET schema_version = 99 WHERE id = 1",
            [],
        )
        .unwrap();
    drop(connection);

    let report = ConfigStore::dry_run_migration_report(&database_path).unwrap();
    assert_eq!(report["outcome"], "refused_newer_database", "{report}");
    assert_eq!(report["stored_version"], json!(99));
    assert_eq!(
        report["current_program_version"],
        json!(CONFIG_SCHEMA_VERSION)
    );
    let error = report["error"].as_object().unwrap();
    assert_eq!(error["kind"], "newer_database");
    assert!(
        error["message"].as_str().unwrap().contains("高于当前程序"),
        "结构化错误必须说明版本过高: {report}"
    );
    assert!(
        error["hint"].as_str().unwrap().contains("不要删除"),
        "结构化错误必须提示保留缓存库: {report}"
    );
    assert!(
        report["backup"]["would_create"].is_null(),
        "被拒绝的库不会再迁移，不能声称会创建备份: {report}"
    );

    let bytes_before = fs::read(&database_path).unwrap();
    let open_error =
        ConfigStore::open(&database_path, &root, bootstrap).expect_err("库比程序新必须拒绝启动");
    let message = format!("{open_error:#}");
    assert!(
        message.contains("高于当前程序") && message.contains("不要删除"),
        "真实打开的错误必须同样结构化: {message}"
    );
    assert_eq!(
        fs::read(&database_path).unwrap(),
        bytes_before,
        "拒绝打开时不得改动配置库"
    );
    assert_eq!(stored_schema_version(&database_path), 99);
    cleanup(&root);
}
