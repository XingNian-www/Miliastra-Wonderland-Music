//! W2/D5：`ConfigStore::save_sections_subset` 的回归测试。
//!
//! 覆盖三件事：
//!
//! 1. CAS 语义与 [`ConfigStore::save`] 一致：过期 `base_revision` 返回 `Conflict`；
//! 2. 只写提交段——其余段的 `value_json` **逐字节**不变（含 revision 列）；
//! 3. `record_history = false` 时 revision 仍 +1，但不新增历史快照行。
//!
//! 与 `tests.rs` 一样用真实临时 SQLite 文件，`stored_*` 系列用独立连接读库，
//! 不用被测代码的读取路径证明自己没错。测试数据全部是占位符。

use super::*;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;

/// 构造测试用最小启动配置（与 `tests::test_bootstrap` 同口径）。
fn test_bootstrap(database_path: &Path) -> BootstrapConfig {
    BootstrapConfig {
        database_path: database_path.to_path_buf(),
        http: crate::config::HttpConfig::default(),
        logging: crate::config::LoggingConfig::default(),
    }
}

/// 创建临时根目录与数据库路径（`deps/data/playback.sqlite3` 是校验要求的文件名）。
fn temp_database(name: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("config-subset-{name}-{}", uuid::Uuid::new_v4()));
    let database_path = root.join("deps/data/playback.sqlite3");
    fs::create_dir_all(database_path.parent().unwrap()).unwrap();
    (root, database_path)
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

/// 读取全部段 `(value_json 原文, revision)`（独立连接），用于逐字节比对。
fn stored_section_rows(database_path: &Path) -> BTreeMap<String, (String, i64)> {
    let connection = rusqlite::Connection::open(database_path).unwrap();
    let mut statement = connection
        .prepare("SELECT section, value_json, revision FROM config_sections")
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                (row.get::<_, String>(1)?, row.get::<_, i64>(2)?),
            ))
        })
        .unwrap();
    rows.collect::<rusqlite::Result<BTreeMap<_, _>>>().unwrap()
}

/// 读取 `config_revisions` 的行数（独立连接）。
fn stored_history_count(database_path: &Path) -> i64 {
    let connection = rusqlite::Connection::open(database_path).unwrap();
    connection
        .query_row("SELECT COUNT(*) FROM config_revisions", [], |row| {
            row.get(0)
        })
        .unwrap()
}

/// 一条合法但可肉眼区分的 identity 段（占位 UID/备注，不含真实玩家数据）。
fn identity_section(uid: &str) -> Value {
    json!({
        "mappings": [{
            "id": "00000000-0000-4000-8000-000000000001",
            "nickname": "示例玩家甲",
            "role": "friend",
            "note": "",
            "real_nickname": "",
            "remark_sourced": false,
            "accounts": [{"game": "genshin", "uid": uid}]
        }]
    })
}

#[test]
fn subset_save_conflicts_on_stale_revision() {
    let (root, database_path) = temp_database("stale-revision");
    let bootstrap = test_bootstrap(&database_path);
    let mut first = ConfigStore::open(&database_path, &root, bootstrap.clone()).unwrap();
    // 第二个独立连接（模拟另一实例/进程）持有同一个库。
    let mut second = ConfigStore::open(&database_path, &root, bootstrap).unwrap();

    let base = first.current_revision().unwrap();
    let outcome = first
        .save_sections_subset(base, &[("identity", identity_section("100000001"))], false)
        .unwrap();
    assert_eq!(outcome.revision, base + 1, "定向写入必须把 revision +1");
    assert_eq!(stored_revision(&database_path), (base + 1) as i64);

    // 第二个实例仍用过期基线：事务内重读 revision，必须冲突且不写库。
    let error = second
        .save_sections_subset(base, &[("identity", identity_section("100000002"))], false)
        .unwrap_err();
    assert!(
        matches!(error, ConfigSaveError::Conflict),
        "过期基线必须返回 Conflict，实际 {error:?}"
    );
    assert_eq!(
        stored_revision(&database_path),
        (base + 1) as i64,
        "冲突时不得写库"
    );

    cleanup(&root);
}

#[test]
fn subset_save_keeps_untouched_sections_byte_identical() {
    let (root, database_path) = temp_database("byte-identical");
    let mut store =
        ConfigStore::open(&database_path, &root, test_bootstrap(&database_path)).unwrap();
    let before = stored_section_rows(&database_path);
    let revision = store.current_revision().unwrap();

    // record_history=true 走完整写入路径（写快照 + 裁剪），仍只能改提交段。
    store
        .save_sections_subset(
            revision,
            &[("identity", identity_section("100000003"))],
            true,
        )
        .unwrap();

    let after = stored_section_rows(&database_path);
    assert_ne!(
        before["identity"], after["identity"],
        "提交段必须真的被写入新值"
    );
    for (section, (value_json, row_revision)) in &before {
        if section == "identity" {
            continue;
        }
        let (after_value_json, after_revision) = &after[section];
        assert_eq!(
            after_value_json, value_json,
            "未提交段 {section} 的 JSON 必须逐字节保持不变"
        );
        assert_eq!(
            after_revision, row_revision,
            "未提交段 {section} 的 revision 列不得被顺手改写"
        );
    }
    assert_eq!(
        stored_revision(&database_path),
        (revision + 1) as i64,
        "提交段写入后 revision 必须 +1"
    );
    assert_eq!(
        stored_history_count(&database_path),
        2,
        "record_history=true 必须新增一条历史快照"
    );

    cleanup(&root);
}

/// `record_history = false`：revision 仍 +1（CAS 需要），但历史行数不变。
///
/// 这条与 `external_chat` 的 `identity_persist_does_not_consume_history` 呼应，
/// 但直接验证 store API 本身。
#[test]
fn subset_save_without_history_does_not_add_revision_rows() {
    let (root, database_path) = temp_database("no-history");
    let mut store =
        ConfigStore::open(&database_path, &root, test_bootstrap(&database_path)).unwrap();
    let base = store.current_revision().unwrap();
    let history_before = stored_history_count(&database_path);

    let outcome = store
        .save_sections_subset(base, &[("identity", identity_section("100000004"))], false)
        .unwrap();

    assert_eq!(outcome.revision, base + 1, "revision 仍必须 +1 供 CAS 使用");
    assert_eq!(
        stored_revision(&database_path),
        (base + 1) as i64,
        "config_meta.revision 必须前进"
    );
    assert_eq!(
        stored_history_count(&database_path),
        history_before,
        "record_history=false 不得新增 config_revisions 行"
    );

    cleanup(&root);
}

fn cleanup(root: &Path) {
    let _ = fs::remove_dir_all(root);
}
