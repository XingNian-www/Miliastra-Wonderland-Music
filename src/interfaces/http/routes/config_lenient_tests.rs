//! 回归：默认（Lenient）启动成功后，HTTP 只读路径必须能正常读到隔离后的配置。
//!
//! 否则面板会报「数据库中的配置未通过校验」——程序起来了但面板用不了。
//! 断言的是底层函数（`config_response` / `load_for_read`），不必起 HTTP 服务器：
//! 鉴权与状态码装配与本次改动无关。夹具全部是占位符。

use super::*;
use serde_json::json;
const PLACEHOLDER_ID_A: &str = "11111111-1111-4111-8111-111111111111";
const PLACEHOLDER_ID_B: &str = "22222222-2222-4222-8222-222222222222";
const PLACEHOLDER_UID: &str = "100000001";

fn open_store() -> (std::path::PathBuf, crate::config::ConfigStore) {
    let root = std::env::temp_dir().join(format!("http-lenient-read-{}", uuid::Uuid::new_v4()));
    let database_path = root.join("deps/data/playback.sqlite3");
    std::fs::create_dir_all(database_path.parent().unwrap()).unwrap();
    let store = crate::config::ConfigStore::open(
        &database_path,
        &root,
        crate::config::BootstrapConfig {
            database_path: database_path.clone(),
            http: crate::config::HttpConfig::default(),
            logging: crate::config::LoggingConfig::default(),
        },
    )
    .expect("测试库必须能初始化");
    (root, store)
}

/// 把重复 UID 的身份段直接写进库：模拟手改 sqlite / 坏 `.bak` 回灌。
fn write_duplicate_identity(store: &crate::config::ConfigStore) {
    let identity = json!({
        "mappings": [
            {
                "nickname": "占位身份甲",
                "id": PLACEHOLDER_ID_A,
                "role": "friend",
                "accounts": [{"game": "genshin", "uid": PLACEHOLDER_UID}]
            },
            {
                "nickname": "占位身份乙",
                "id": PLACEHOLDER_ID_B,
                "role": "admin",
                "accounts": [{"game": "genshin", "uid": PLACEHOLDER_UID}]
            }
        ]
    });
    let connection = rusqlite::Connection::open(&store.bootstrap().database_path).unwrap();
    connection
        .execute(
            "UPDATE config_sections SET value_json = ?1 WHERE section = 'identity'",
            rusqlite::params![serde_json::to_string(&identity).unwrap()],
        )
        .unwrap();
}

#[test]
fn http_config_read_works_after_lenient_startup() {
    let (root, store) = open_store();
    write_duplicate_identity(&store);

    // 1) 启动链：默认模式（`LoadMode::default()`）下成功，坏条目进 quarantine。
    // 用默认值本身：默认值一旦改回 Strict，这里立刻变红。
    assert_eq!(
        crate::config::LoadMode::default(),
        crate::config::LoadMode::Lenient,
        "默认加载模式必须是 Lenient"
    );
    let guard = crate::config::test_mode_override::scoped(crate::config::LoadMode::default());
    let started = store.load_full_current().expect("默认模式启动必须成功");
    started.validate().expect("启动后的配置必须通过校验");

    // 2) HTTP 只读路径（ai / commands / config 共用的 load_for_read）。
    let loaded = load_for_read(&store).expect("Lenient 启动后只读加载不得失败");
    loaded.validate().expect("只读加载的配置必须通过校验");

    // 3) GET /config 的响应体：既有字段不变，新增容错状态。
    let response = config_response(&store).expect("Lenient 启动后读取 /config 不得报校验失败");
    drop(guard);

    assert_eq!(response["loadMode"], json!("lenient"));
    assert_eq!(response["quarantinedCount"], json!(1));
    let quarantined = response["quarantined"]
        .as_array()
        .expect("quarantined 必须是数组");
    assert_eq!(quarantined.len(), 1);
    assert_eq!(quarantined[0]["elementRef"], json!("identity.mappings[1]"));
    assert!(
        quarantined[0]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("账号重复")),
        "面板必须能直接读到隔离原因: {}",
        quarantined[0]["reason"]
    );
    // 只增不改：既有字段原样保留。
    assert_eq!(response["ok"], json!(true));
    assert!(response["sections"].is_object());
    assert!(response["revision"].is_number());
    assert_eq!(
        response["schemaVersion"],
        json!(crate::config::CONFIG_SCHEMA_VERSION)
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn strict_read_path_still_rejects_the_same_database() {
    // 变异对照：只读路径若回到严格口径，同一份库必然报校验失败。
    let (root, store) = open_store();
    write_duplicate_identity(&store);

    let guard = crate::config::test_mode_override::scoped(crate::config::LoadMode::Strict);
    let error = load_for_read(&store).expect_err("strict 只读路径必须拒绝坏库");
    assert!(
        format!("{error:?}").contains("500") || error.message.contains("校验"),
        "必须是内部错误而不是静默放行: {error:?}"
    );
    drop(guard);
    std::fs::remove_dir_all(root).ok();
}
