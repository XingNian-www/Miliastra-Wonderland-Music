//! W5/D7 容错半的回归测试：`LoadMode`、4 类用户容器隔离、quarantine 表与剪枝找回。
//!
//! 测试一律用真实临时 SQLite 文件：坏数据必须是**库里已有的**（而不是内存里
//! 构造的 `AppConfig`），这样才覆盖「启动 → 反序列化 → 校验 → 隔离 → 写 quarantine」
//! 的完整链路。所有密钥/UID 都是占位符。

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use uuid::Uuid;

use super::*;
use crate::config::migrations;
use crate::features::identity::IdentityAccess;

const ORIGINAL_ID: &str = "11111111-1111-4111-8111-111111111111";
const DUPLICATE_ID: &str = "22222222-2222-4222-8222-222222222222";
const DUPLICATE_UID: &str = "100000001";

fn bootstrap(database_path: &Path) -> BootstrapConfig {
    BootstrapConfig {
        database_path: database_path.to_path_buf(),
        http: crate::config::HttpConfig::default(),
        logging: crate::config::LoggingConfig::default(),
    }
}

fn temp_database(name: &str) -> (PathBuf, PathBuf) {
    let root =
        std::env::temp_dir().join(format!("config-quarantine-{name}-{}", uuid::Uuid::new_v4()));
    let database_path = root.join("deps/data/playback.sqlite3");
    std::fs::create_dir_all(database_path.parent().unwrap()).unwrap();
    (root, database_path)
}

fn open_store(name: &str) -> (PathBuf, PathBuf, ConfigStore) {
    let (root, database_path) = temp_database(name);
    let store = ConfigStore::open(&database_path, &root, bootstrap(&database_path))
        .expect("测试库必须能初始化");
    (root, database_path, store)
}

fn cleanup(root: &Path) {
    let _ = std::fs::remove_dir_all(root);
}

/// 用独立 SQL 写段：模拟用户手改 sqlite / 坏 `.bak` 回灌，而不是走保存校验。
fn write_section(store: &ConfigStore, section: &str, value: &Value) {
    store
        .connection
        .execute(
            "UPDATE config_sections SET value_json = ?1 WHERE section = ?2",
            rusqlite::params![serde_json::to_string(value).unwrap(), section],
        )
        .unwrap();
}

fn read_section_from(database_path: &Path, section: &str) -> Value {
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

/// 把库降级成旧 schema 版本，重开时触发 v3 → v4 迁移（含末尾剪枝）。
fn write_schema_version(database_path: &Path, version: i64) {
    let connection = rusqlite::Connection::open(database_path).unwrap();
    connection
        .execute(
            "UPDATE config_meta SET schema_version = ?1 WHERE id = 1",
            rusqlite::params![version],
        )
        .unwrap();
}

/// 两条映射绑定同一个 UID：一条「原身份」+ 一条重复的「重复身份」。
fn duplicate_identity_section() -> Value {
    json!({
        "mappings": [
            {
                "nickname": "原身份",
                "id": ORIGINAL_ID,
                "role": "friend",
                "accounts": [{"game": "genshin", "uid": DUPLICATE_UID}]
            },
            {
                "nickname": "重复身份",
                "id": DUPLICATE_ID,
                "role": "admin",
                "accounts": [{"game": "genshin", "uid": DUPLICATE_UID}]
            }
        ]
    })
}

#[test]
fn lenient_load_isolates_duplicate_identity() {
    let (root, _database_path, store) = open_store("lenient-duplicate-identity");
    write_section(&store, "identity", &duplicate_identity_section());
    // 严格原语（load_full）必须与重构前一致：重复 UID 直接拒绝启动。
    let strict_error = store.load_full().expect_err("Strict 必须拒绝重复 UID");
    assert!(
        format!("{strict_error:#}").contains("账号重复"),
        "Strict 错误必须指出重复账号: {strict_error:#}"
    );
    assert!(
        store.quarantine_entries().unwrap().is_empty(),
        "Strict 模式不得写 quarantine"
    );

    let loaded = store
        .load_full_with_mode(LoadMode::Lenient)
        .expect("Lenient 必须隔离坏映射并加载成功");

    let entries = store.quarantine_entries().unwrap();
    assert_eq!(entries.len(), 1, "只隔离重复的那一条映射: {entries:?}");
    assert_eq!(entries[0].section, "identity");
    assert_eq!(entries[0].element_ref, "identity.mappings[1]");
    assert!(
        entries[0].reason.contains("账号重复"),
        "隔离原因必须是原始校验错误: {}",
        entries[0].reason
    );

    // 被隔离的是靠后的重复条目：它不能通过该 UID 获得任何授权。
    let directory = IdentityAccess::new(loaded.identity);
    let original_id = Uuid::parse_str(ORIGINAL_ID).unwrap();
    let duplicate_id = Uuid::parse_str(DUPLICATE_ID).unwrap();
    assert_eq!(
        directory.resolve("原身份").map(|identity| identity.id),
        Some(original_id),
        "靠前的身份必须保留"
    );
    assert!(
        directory.resolve("重复身份").is_none(),
        "被隔离的重复身份不得按备注解析到"
    );
    assert_eq!(
        directory
            .resolve_account("genshin", DUPLICATE_UID)
            .map(|identity| identity.id),
        Some(original_id),
        "该 UID 只能解析到保留的那条身份，不能解析到重复条目"
    );
    assert_ne!(original_id, duplicate_id);
    cleanup(&root);
}

#[test]
fn lenient_load_quarantines_duplicate_account_within_one_mapping() {
    // 同一条映射内部重复绑定同一个 UID：整条映射被隔离，该 UID 不再由任何人授权。
    let (root, _database_path, store) = open_store("lenient-duplicate-account");
    let identity = json!({
        "mappings": [
            {
                "nickname": "内部重复",
                "id": ORIGINAL_ID,
                "role": "friend",
                "accounts": [
                    {"game": "genshin", "uid": DUPLICATE_UID},
                    {"game": "genshin", "uid": DUPLICATE_UID}
                ]
            },
            {
                "nickname": "无辜身份",
                "id": DUPLICATE_ID,
                "role": "friend",
                "accounts": [{"game": "genshin", "uid": "100000002"}]
            }
        ]
    });
    write_section(&store, "identity", &identity);

    let loaded = store
        .load_full_with_mode(LoadMode::Lenient)
        .expect("Lenient 必须隔离内部重复账号的映射");
    let entries = store.quarantine_entries().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].element_ref, "identity.mappings[0]");
    let directory = IdentityAccess::new(loaded.identity);
    assert!(
        directory
            .resolve_account("genshin", DUPLICATE_UID)
            .is_none(),
        "重复 UID 的整条映射被隔离后，该 UID 不授权"
    );
    assert!(
        directory.resolve("无辜身份").is_some(),
        "其余身份必须仍然可用"
    );
    cleanup(&root);
}

#[test]
fn lenient_load_isolates_invalid_workflow() {
    let (root, _database_path, store) = open_store("lenient-invalid-workflow");
    let mut custom = read_section_from(&store.bootstrap().database_path, "custom_workflows");
    let enabled_before = custom["workflows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|workflow| workflow["enabled"] == json!(true))
        .count();
    let workflows = custom["workflows"].as_array_mut().unwrap();
    let bad_index = workflows.len();
    workflows.push(json!({
        "name": "坏流程",
        "enabled": true,
        "commands": ["坏流程"],
        "allow_args": false,
        "message_types": ["pink"],
        "confirm_before_run": false,
        "confirm_message": "",
        "confirm_message_types": [],
        "success_message": "",
        "steps": [{"type": "no_such_step"}]
    }));
    write_section(&store, "custom_workflows", &custom);
    assert!(
        store.load_full().is_err(),
        "Strict 必须拒绝未知 workflow step"
    );

    let loaded = store
        .load_full_with_mode(LoadMode::Lenient)
        .expect("Lenient 必须禁用坏 workflow 并加载成功");
    let entries = store.quarantine_entries().unwrap();
    assert_eq!(entries.len(), 1, "只隔离一条 workflow: {entries:?}");
    assert_eq!(entries[0].section, "custom_workflows");
    assert_eq!(
        entries[0].element_ref,
        format!("custom_workflows.workflows[{bad_index}]")
    );
    assert!(
        entries[0]
            .reason
            .contains("unsupported custom workflow step type"),
        "隔离原因必须指出未知 step: {}",
        entries[0].reason
    );

    let bad = &loaded.custom_workflows.workflows[bad_index];
    assert!(!bad.enabled, "坏 workflow 必须被禁用");
    assert_eq!(
        bad.steps[0].step_type, "no_such_step",
        "脚本原文必须保留，用户修好后可直接重新启用"
    );
    let enabled_after = loaded
        .custom_workflows
        .workflows
        .iter()
        .filter(|workflow| workflow.enabled)
        .count();
    assert_eq!(
        enabled_after, enabled_before,
        "除被禁用的那条外，其余流程必须保持原状"
    );
    assert!(
        loaded
            .custom_workflows
            .workflows
            .iter()
            .any(|workflow| workflow.enabled && workflow.name == "鼠标中键"),
        "内置流程仍必须可用"
    );
    cleanup(&root);
}

#[test]
fn lenient_load_ignores_unknown_downsample_key() {
    let (root, _database_path, store) = open_store("lenient-downsample");
    let mut templates = read_section_from(&store.bootstrap().database_path, "templates");
    templates["panel_downsample_factors"] = json!({"no_such_purpose": 2});
    write_section(&store, "templates", &templates);
    assert!(
        store.load_full().is_err(),
        "Strict 必须拒绝未知模板倍数用途"
    );

    let loaded = store
        .load_full_with_mode(LoadMode::Lenient)
        .expect("Lenient 必须忽略未知倍数用途并加载成功");
    assert!(
        loaded.templates.panel_downsample_factors.is_empty(),
        "未知用途必须从运行态配置里消失"
    );
    let entries = store.quarantine_entries().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].element_ref,
        "templates.panel_downsample_factors.no_such_purpose"
    );
    cleanup(&root);
}

#[test]
fn strict_load_still_fails_on_duplicate_identity() {
    // 严格原语的行为不变：显式走 load_full()（= Strict）时，同一份坏数据仍然启动失败，
    // 且不产生任何 quarantine 副作用。默认值翻转**没有**削弱这条断言——
    // 严格路径仍然逐字节保持重构前的失败行为。
    let (root, _database_path, store) = open_store("strict-duplicate-identity");
    write_section(&store, "identity", &duplicate_identity_section());
    let error = store
        .load_full()
        .expect_err("load_full（Strict 原语）必须保持重构前的失败行为");
    assert!(
        format!("{error:#}").contains("数据库中的配置未通过校验"),
        "错误链必须保留原有上下文: {error:#}"
    );
    assert!(
        format!("{error:#}").contains("账号重复"),
        "错误必须指出根因: {error:#}"
    );
    assert!(
        store.quarantine_entries().unwrap().is_empty(),
        "Strict 失败不得写 quarantine"
    );
    cleanup(&root);
}

#[test]
fn quarantine_records_element_ref_and_original_json() {
    let (root, _database_path, store) = open_store("quarantine-element-ref");
    let mut access = read_section_from(&store.bootstrap().database_path, "command_access");
    access["overrides"] = json!({
        "@不存在的命令": {"enabled": true, "minimum_role": "user"}
    });
    write_section(&store, "command_access", &access);
    assert!(store.load_full().is_err(), "Strict 必须拒绝未知命令覆盖");

    let loaded = store
        .load_full_with_mode(LoadMode::Lenient)
        .expect("Lenient 必须隔离未知命令覆盖");
    assert!(
        loaded.command_access.overrides.is_empty(),
        "未知命令覆盖必须从运行态配置里消失"
    );

    let entries = store.quarantine_entries().unwrap();
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry.section, "command_access");
    assert_eq!(
        entry.element_ref, "command_access.overrides.@不存在的命令",
        "element_ref 必须定位到具体键"
    );
    assert!(
        entry.reason.contains("未知命令ID"),
        "reason 必须是原始校验错误: {}",
        entry.reason
    );
    let original: Value = serde_json::from_str(&entry.original_json).unwrap();
    assert_eq!(
        original,
        json!({"enabled": true, "minimum_role": "user"}),
        "original_json 必须保留被隔离条目的原文（非 secret 字段不掩码）"
    );
    assert!(entry.created_at_ms > 0, "created_at_ms 必须落库");
    cleanup(&root);
}

#[test]
fn lenient_load_does_not_relax_structural_errors() {
    // 结构错误（字段被删除/改名、类型不符）在两种模式下都严格：
    // Lenient 只处理 4 类用户容器，不掩盖结构损坏。
    let (root, _database_path, store) = open_store("lenient-structural-strict");
    let mut window = read_section_from(&store.bootstrap().database_path, "window");
    window["legacy_structural_field"] = json!(1);
    write_section(&store, "window", &window);

    let strict_error = store.load_full().expect_err("Strict 必须拒绝结构错误");
    let lenient_error = store
        .load_full_with_mode(LoadMode::Lenient)
        .expect_err("Lenient 也必须拒绝结构错误");
    assert!(
        format!("{lenient_error:#}").contains("legacy_structural_field")
            || format!("{lenient_error:#}").contains("字段被删除或改名"),
        "错误必须能定位到多余字段: {lenient_error:#}"
    );
    assert!(
        format!("{strict_error:#}").contains("legacy_structural_field")
            || format!("{strict_error:#}").contains("字段被删除或改名"),
        "Strict 错误必须能定位到多余字段: {strict_error:#}"
    );
    assert!(
        store.quarantine_entries().unwrap().is_empty(),
        "结构错误不得被写进 quarantine"
    );
    cleanup(&root);
}

#[test]
fn persisted_shape_errors_are_collected_per_element() {
    // 分段收集：两个段里的 3 个多余键一次性全部报出来（旧实现只报第一个）。
    let stored = json!({
        "window": {"keep": 1, "gone_a": 1, "gone_b": 2},
        "queue": {"max_size": 10, "gone_c": 3},
        "templates": {"items": [{"ok": 1}, {"ok": 2, "bogus": true}]}
    });
    let canonical = json!({
        "window": {"keep": 1},
        "queue": {"max_size": 10},
        "templates": {"items": [{"ok": 1}, {"ok": 2}]}
    });
    let error = codec::validate_persisted_shape(&stored, &canonical, "").unwrap_err();
    let structured = error
        .downcast_ref::<types::ConfigFieldErrors>()
        .expect("形状错误必须结构化为 ConfigFieldErrors");
    let fields = structured
        .errors
        .iter()
        .map(|error| error.field.as_str())
        .collect::<Vec<_>>();
    // serde_json::Map 默认按对象键排序，遍历顺序即段名字典序。
    assert_eq!(
        fields,
        vec![
            "queue.gone_c",
            "templates.items[1].bogus",
            "window.gone_a",
            "window.gone_b"
        ],
        "必须收集全部元素错误而不是首个即返回"
    );
    let sections = structured
        .errors
        .iter()
        .map(|error| error.section.as_str())
        .collect::<Vec<_>>();
    assert_eq!(sections, vec!["queue", "templates", "window", "window"]);
    // 数组里的错误带下标定位，对象键带键名定位。
    let array_error = &structured.errors[1];
    let element_ref = array_error.element_ref.as_ref().expect("必须带 ElementRef");
    assert_eq!(element_ref.index, Some(1));
    assert_eq!(element_ref.key.as_deref(), Some("bogus"));
    assert_eq!(element_ref.path, "templates.items[1].bogus");
    let object_error = &structured.errors[2];
    assert_eq!(
        object_error
            .element_ref
            .as_ref()
            .and_then(|element| element.key.as_deref()),
        Some("gone_a")
    );
}

#[test]
fn structured_field_errors_survive_context_wrapping() {
    // from_db_value 会给形状错误加一层 context；save 路径必须仍能 downcast 出
    // 完整的元素级错误列表，而不是被压平成一条字符串。
    let error = codec::validate_persisted_shape(
        &json!({"window": {"keep": 1, "gone_a": 1, "gone_b": 2}}),
        &json!({"window": {"keep": 1}}),
        "",
    )
    .unwrap_err();
    let wrapped: anyhow::Error = Err::<(), _>(error)
        .context("从配置数据库组装完整配置失败")
        .unwrap_err();
    let errors = types::field_errors_from_error(&wrapped);
    assert_eq!(errors.errors.len(), 2, "两层错误都必须保留: {errors}");
    assert_eq!(errors.errors[0].section, "window");
    assert!(errors.errors[0].element_ref.is_some());
}

#[test]
fn migration_prune_records_quarantine_before_delete() {
    // 迁移剪枝：未知旧键先被记进 quarantine（可找回），再从配置库里删除。
    let (root, database_path, store) = open_store("prune-quarantine");
    let mut templates = read_section_from(&database_path, "templates");
    templates["legacy_retired_key"] = json!("用户旧值-占位");
    write_section(&store, "templates", &templates);
    drop(store);

    // 降级成 v3 后重开：v3 → v4 迁移末尾会剪掉未知键。
    write_schema_version(&database_path, 3);
    let store = ConfigStore::open(&database_path, &root, bootstrap(&database_path))
        .expect("v3 库必须能平滑迁移");

    let stored = read_section_from(&database_path, "templates");
    assert!(
        stored.get("legacy_retired_key").is_none(),
        "剪枝仍然必须真的删掉旧键: {stored}"
    );
    let entries = store.quarantine_entries().unwrap();
    let entry = entries
        .iter()
        .find(|entry| entry.element_ref == "templates.legacy_retired_key")
        .unwrap_or_else(|| panic!("剪枝必须先把旧键写进 quarantine: {entries:?}"));
    assert_eq!(entry.section, "templates");
    assert!(
        entry.reason.contains("不再被当前程序声明"),
        "reason 必须说明剪枝原因: {}",
        entry.reason
    );
    let original: Value = serde_json::from_str(&entry.original_json).unwrap();
    assert_eq!(
        original,
        json!("用户旧值-占位"),
        "original_json 必须可找回原文"
    );
    // 迁移本身仍然成功、旧库照常可加载。
    store.load_full().expect("迁移后的库必须能正常加载");
    cleanup(&root);
}

#[test]
fn quarantine_records_are_masked_before_persisting() {
    // 隐私：original_json 落库前必须复用 SECRET_PATHS 掩码。
    let (root, _database_path, store) = open_store("quarantine-mask");
    let records = [
        super::quarantine::QuarantineRecord {
            section: "ai".to_string(),
            element_ref: "ai.api_key".to_string(),
            reason: "测试掩码（占位符）".to_string(),
            original_json: json!("sk-test-placeholder"),
        },
        super::quarantine::QuarantineRecord {
            section: "ai".to_string(),
            element_ref: "ai".to_string(),
            reason: "整段被移除（占位符）".to_string(),
            original_json: json!({"api_key": "sk-test-placeholder", "provider": "placeholder"}),
        },
    ];
    super::quarantine::store_records(&store.connection, &records).expect("写 quarantine 必须成功");

    let entries = store.quarantine_entries().unwrap();
    assert_eq!(entries.len(), 2);
    for entry in &entries {
        assert!(
            !entry.original_json.contains("sk-test-placeholder"),
            "secret 字段必须已掩码: {}",
            entry.original_json
        );
        assert!(
            entry.original_json.contains(SECRET_MASK),
            "必须复用既有 SECRET_MASK: {}",
            entry.original_json
        );
    }
    let section_original: Value = serde_json::from_str(&entries[1].original_json).unwrap();
    assert_eq!(section_original["api_key"], json!(SECRET_MASK));
    assert_eq!(section_original["provider"], json!("placeholder"));
    cleanup(&root);
}

#[test]
fn default_load_mode_is_lenient() {
    // 行为变更（用户已批准）：默认宽松。未设置 / 空串 / 纯空白都是默认。
    assert_eq!(
        LoadMode::default(),
        LoadMode::Lenient,
        "默认加载模式必须是 Lenient"
    );
    assert_eq!(LoadMode::from_env_value(None), LoadMode::Lenient);
    assert_eq!(LoadMode::from_env_value(Some("")), LoadMode::Lenient);
    assert_eq!(LoadMode::from_env_value(Some("   ")), LoadMode::Lenient);
    assert_eq!(LoadMode::from_env_value(Some("lenient")), LoadMode::Lenient);
    assert_eq!(LoadMode::from_env_value(Some("LENIENT")), LoadMode::Lenient);
    assert_eq!(
        LoadMode::from_env_value(Some(" lenient ")),
        LoadMode::Lenient
    );
    // 机器可读名是面板契约：改文案不能让 `loadMode` 字段失配。
    assert_eq!(LoadMode::Lenient.as_str(), "lenient");
    assert_eq!(LoadMode::Strict.as_str(), "strict");
    assert_eq!(LoadMode::Lenient.is_lenient(), true);
    assert_eq!(LoadMode::Strict.is_lenient(), false);
}

#[test]
fn strict_can_still_be_requested_by_env() {
    // 回退路径：显式 `strict` 仍然严格，行为与重构前一致。
    assert_eq!(LoadMode::from_env_value(Some("strict")), LoadMode::Strict);
    assert_eq!(LoadMode::from_env_value(Some("STRICT")), LoadMode::Strict);
    assert_eq!(
        LoadMode::from_env_value(Some("  Strict  ")),
        LoadMode::Strict
    );

    // 真环境变量路径（不污染进程状态：只读，不改写）。测试进程里若被测框架
    // 设了该变量，`from_env()` 必须如实反映它；未设置时如实回落到默认。
    let from_process_env = LoadMode::from_env();
    match std::env::var(LoadMode::ENV).ok() {
        Some(value) => assert_eq!(from_process_env, LoadMode::from_env_value(Some(&value))),
        None => assert_eq!(from_process_env, LoadMode::default()),
    }

    // 进程级入口的单点语义：用线程局部覆盖断言「请求 strict 就真的严格」，
    // 不依赖进程级 OnceLock 的首次读取（并行测试下顺序不确定）。
    let guard = crate::config::test_mode_override::scoped(LoadMode::Strict);
    assert_eq!(LoadMode::current(), LoadMode::Strict);
    assert!(!LoadMode::current().is_lenient());
    drop(guard);
}

#[test]
fn unknown_env_value_falls_back_to_lenient() {
    // 拼错的值不能把用户直接挡在启动门外：回落默认（Lenient），并在日志里告警。
    assert_eq!(LoadMode::from_env_value(Some("1")), LoadMode::Lenient);
    assert_eq!(LoadMode::from_env_value(Some("true")), LoadMode::Lenient);
    assert_eq!(
        LoadMode::from_env_value(Some("lennient")),
        LoadMode::Lenient
    );
    // 回落目标就是默认值本身：今后改默认时解析分支不会分叉。
    assert_eq!(
        LoadMode::from_env_value(Some("garbage")),
        LoadMode::default()
    );
}

#[test]
fn process_mode_is_cached_after_first_read() {
    // `current()` 是进程级单点：首次读取后固定，运行中改环境变量不得让启动链
    // 与读路径产生分歧。线程局部覆盖关闭时，同一进程内多次调用必须一致。
    let first = LoadMode::current();
    assert_eq!(LoadMode::current(), first, "current() 必须缓存首次读取结果");
    // 覆盖生效期间返回值跟随覆盖，松开即恢复缓存值——这正是「单点」的可观测形态。
    {
        let guard = crate::config::test_mode_override::scoped(LoadMode::Strict);
        assert_eq!(LoadMode::current(), LoadMode::Strict);
        assert_ne!(LoadMode::current(), first, "覆盖必须立刻生效");
        drop(guard);
    }
    assert_eq!(LoadMode::current(), first, "覆盖结束后必须回到进程级缓存值");
}

#[test]
fn duplicate_identity_no_longer_blocks_startup_by_default() {
    // 行为变更的核心证据：**默认模式**下重复 UID 不再是启动失败，而是被隔离。
    let (root, _database_path, store) = open_store("default-lenient-duplicate");
    write_section(&store, "identity", &duplicate_identity_section());

    // 用 `LoadMode::default()` 本身作为模式：既走真实默认语义，又不受宿主机
    // 是否设置了 MILIASTRA_CONFIG_LOAD_MODE 影响（环境变量在别的测试里也会被读）。
    // 这条断言对 `#[default]` 直接敏感：默认值一旦改回 Strict 即红。
    assert_eq!(
        LoadMode::default(),
        LoadMode::Lenient,
        "默认值必须已翻转为 Lenient"
    );
    let guard = crate::config::test_mode_override::scoped(LoadMode::default());
    let loaded = store
        .load_full_current()
        .expect("默认模式必须隔离重复 UID 并成功启动");
    drop(guard);

    let entries = store.quarantine_entries().unwrap();
    assert_eq!(entries.len(), 1, "必须恰好隔离重复的那一条: {entries:?}");
    assert_eq!(entries[0].element_ref, "identity.mappings[1]");

    // 隔离之后主流程拿到的配置本身是**能通过校验**的：这正是面板不报错的前提。
    loaded.validate().expect("隔离后的配置必须完整通过校验");

    // 被隔离的条目不能凭重复 UID 获得授权；靠前的那条仍然生效。
    let directory = IdentityAccess::new(loaded.identity);
    assert_eq!(
        directory
            .resolve_account("genshin", DUPLICATE_UID)
            .map(|identity| identity.id),
        Some(Uuid::parse_str(ORIGINAL_ID).unwrap())
    );
    assert!(directory.resolve("重复身份").is_none());
    cleanup(&root);
}

#[test]
fn lenient_mode_does_not_weaken_startup_critical_errors() {
    // 翻转默认值**没有**放松启动关键项：结构错误在默认模式下仍然拒绝启动。
    let (root, _database_path, store) = open_store("default-lenient-structural");
    let mut window = read_section_from(&store.bootstrap().database_path, "window");
    window["legacy_structural_field"] = json!(1);
    write_section(&store, "window", &window);

    let guard = crate::config::test_mode_override::scoped(LoadMode::Lenient);
    let error = store
        .load_full_current()
        .expect_err("默认 Lenient 也必须拒绝结构错误");
    drop(guard);
    assert!(
        format!("{error:#}").contains("legacy_structural_field")
            || format!("{error:#}").contains("字段被删除或改名"),
        "错误必须能定位到多余字段: {error:#}"
    );
    assert!(
        store.quarantine_entries().unwrap().is_empty(),
        "结构错误不得被写进 quarantine"
    );
    cleanup(&root);
}

#[test]
fn strict_mode_blocks_the_same_database() {
    // 变异对照：同一份坏库在 strict 下仍然启动失败，证明「默认模式下能启动」
    // 是模式翻转的功劳，而不是坏数据根本没写进库。
    let (root, _database_path, store) = open_store("strict-after-default");
    write_section(&store, "identity", &duplicate_identity_section());

    let guard = crate::config::test_mode_override::scoped(LoadMode::Strict);
    let error = store
        .load_full_current()
        .expect_err("strict 模式必须拒绝启动");
    drop(guard);

    assert!(
        format!("{error:#}").contains("账号重复"),
        "错误必须指出根因: {error:#}"
    );
    assert!(
        store.quarantine_entries().unwrap().is_empty(),
        "strict 不得写 quarantine"
    );
    // 严格原语同样失败：默认值翻转没有把严格路径也放松。
    assert!(store.load_full().is_err(), "strict 下 load_full 必须失败");
    cleanup(&root);
}

#[test]
fn quarantine_table_is_created_without_bumping_schema_version() {
    // 不升 CONFIG_SCHEMA_VERSION：新表只是附加表，三表三态判定不受影响。
    let (root, database_path, store) = open_store("quarantine-schema");
    let version: i64 = store
        .connection
        .query_row(
            "SELECT schema_version FROM config_meta WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(version, CONFIG_SCHEMA_VERSION);
    let connection = rusqlite::Connection::open(&database_path).unwrap();
    let exists: bool = connection
        .prepare(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'config_quarantine'",
        )
        .unwrap()
        .exists([])
        .unwrap();
    assert!(exists, "config_quarantine 必须由 open() 补建");
    // 迁移记录入口也可用（空库没有剪枝，读取为空而不是报错）。
    assert!(store.quarantine_entries().unwrap().is_empty());
    cleanup(&root);
}

/// 迁移模块的 `PrunedEntry` 是纯数据结构：这里顺手确认它的字段契约没被改坏，
/// 避免剪枝测试只靠 SQL 断言时漏掉结构变化。
#[test]
fn pruned_entry_keeps_section_and_element_ref() {
    let entry = migrations::PrunedEntry {
        section: "templates".to_string(),
        element_ref: "templates.legacy".to_string(),
        reason: "测试".to_string(),
        original_json: json!(1),
    };
    assert_eq!(entry.section, "templates");
    assert_eq!(entry.element_ref, "templates.legacy");
}
