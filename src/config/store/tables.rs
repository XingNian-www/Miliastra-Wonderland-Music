//! 配置三表的**期望结构**与结构校验。
//!
//! 校验只读 SQLite 自己的元数据，不做「建表 SQL 里有没有某个子串」的猜测：
//!
//! - 列：`PRAGMA table_info` 的列名 + 声明类型 + NOT NULL 标志，列集合必须完全
//!   一致（顺序不限）；
//! - 主键：优先 `PRAGMA index_list` 找 `origin = 'pk'` 的索引再 `index_info` 取列；
//!   `INTEGER PRIMARY KEY` 单列（rowid 别名）不产生索引时回退 `table_info.pk`；
//! - CHECK：从 `sqlite_master.sql` 用最小 SQL 词法扫描器取出 CHECK 表达式，
//!   规范化后必须**精确等于** `check(id=1)`——`IN (...)`、`OR` 这类放宽一律拒绝。
//!
//! 词法扫描器（[`skip_lexical_element`]）存在的理由：建表 SQL 里可能出现字符串
//! 常量、注释、引用标识符，其中的 `check` 字样与括号**不能**参与匹配。

use anyhow::Result;
use rusqlite::OptionalExtension;

/// 配置三表的名字，供 `ensure_schema` 做「全在 / 全不在 / 部分在」三态判定。
pub(super) const CONFIG_TABLES: [&str; 3] = ["config_meta", "config_sections", "config_revisions"];

/// 容错隔离表（W5/D7）：坏条目先记在这里再被忽略/删除，用户事后可找回。
///
/// **不升 [`super::sqlite::CONFIG_SCHEMA_VERSION`]**：本表只由
/// `ConfigStore::open` 用 `CREATE TABLE IF NOT EXISTS` 补建，不参与配置三表的
/// 三态判定（`CONFIG_TABLES` 仍只有三张），也不影响任何版本分支。
pub(super) const QUARANTINE_TABLE: &str = "config_quarantine";

/// 期望列：(列名, SQLite 声明类型, 是否要求 NOT NULL)。
pub(super) const QUARANTINE_COLUMNS: [(&str, &str, bool); 6] = [
    ("id", "INTEGER", false),
    ("section", "TEXT", true),
    ("element_ref", "TEXT", true),
    ("reason", "TEXT", true),
    ("original_json", "TEXT", true),
    ("created_at_ms", "INTEGER", true),
];

/// 确保 `config_quarantine` 存在；表结构不符合期望时**只告警不阻断启动**。
///
/// 取舍：这是一张纯附加的“找回”表，坏掉它不应该让用户连程序都起不来；
/// 写 quarantine 的调用点会把写入失败降级为 warn，原库数据与迁移 `.bak` 仍在。
pub(super) fn ensure_quarantine_table(connection: &rusqlite::Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS config_quarantine (
                id INTEGER PRIMARY KEY,
                section TEXT NOT NULL,
                element_ref TEXT NOT NULL,
                reason TEXT NOT NULL,
                original_json TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL
            );",
    )?;
    if !table_structure_matches(connection, QUARANTINE_TABLE, &QUARANTINE_COLUMNS)? {
        log::warn!(
            "容错隔离表 {QUARANTINE_TABLE} 的结构与当前版本不一致：本次启动的隔离记录可能无法写入，请检查该表（不会影响配置三表与启动）"
        );
    }
    Ok(())
}

/// 期望列：(列名, SQLite 声明类型, 是否要求 NOT NULL)。
///
/// 主键列（INTEGER/TEXT PRIMARY KEY）在 PRAGMA table_info 中 notnull 为 0，
/// 按 SQLite 实际行为声明；其余列均声明 NOT NULL。
pub(super) const META_COLUMNS: [(&str, &str, bool); 4] = [
    ("id", "INTEGER", false),
    ("schema_version", "INTEGER", true),
    ("revision", "INTEGER", true),
    ("updated_at_ms", "INTEGER", true),
];
pub(super) const SECTIONS_COLUMNS: [(&str, &str, bool); 4] = [
    ("section", "TEXT", false),
    ("value_json", "TEXT", true),
    ("revision", "INTEGER", true),
    ("updated_at_ms", "INTEGER", true),
];
pub(super) const REVISIONS_COLUMNS: [(&str, &str, bool); 3] = [
    ("revision", "INTEGER", false),
    ("snapshot_json", "TEXT", true),
    ("created_at_ms", "INTEGER", true),
];

/// 检查表是否存在且列结构匹配期望列（列集合必须完全一致，顺序不限）；
/// 每列同时校验列名、SQLite 声明类型（declared type，比较忽略大小写）与
/// NOT NULL 标志（notnull=1）。表缺失或任一不匹配时返回 false。
///
/// `expected_columns` 元素为 (列名, 声明类型, 是否要求 NOT NULL)；主键列在
/// PRAGMA table_info 中 notnull 为 0，期望值应按 SQLite 实际行为声明。
pub(super) fn table_structure_matches(
    connection: &rusqlite::Connection,
    table: &str,
    expected_columns: &[(&str, &str, bool)],
) -> Result<bool> {
    let exists: bool = connection
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?1")?
        .exists(rusqlite::params![table])?;
    if !exists {
        return Ok(false);
    }
    // 实际列：(列名, 声明类型, notnull 标志)；PRAGMA table_info 的 type 为
    // 建表时声明的原文（SQLite 不做大小写归一），比较时统一转小写。
    let mut columns = Vec::new();
    {
        let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)? != 0,
            ))
        })?;
        for row in rows {
            columns.push(row?);
        }
    }
    if columns.len() != expected_columns.len() {
        return Ok(false);
    }
    // 逐期望列匹配：列名一致、声明类型大小写不敏感一致、NOT NULL 标志一致。
    for (expected_name, expected_type, expected_not_null) in expected_columns {
        let Some((_, declared_type, not_null)) = columns
            .iter()
            .find(|(name, _, _)| name.as_str() == *expected_name)
        else {
            return Ok(false);
        };
        if !declared_type.eq_ignore_ascii_case(expected_type) || *not_null != *expected_not_null {
            return Ok(false);
        }
    }
    Ok(true)
}

/// 校验 config_meta 的约束（基于 SQLite 自身元数据，不做字符串包含猜测）：
///
/// 1. 主键绑定：优先用 `PRAGMA index_list` 找 `origin = 'pk'` 的索引，
///    再用 `PRAGMA index_info` 取其列，主键必须恰好是 `id` 一列；
///    `id INTEGER PRIMARY KEY` 单列是 rowid 别名，index_list 不返回行，
///    此时回退 `PRAGMA table_info` 的 `pk` 列序（pk > 0 表示属于主键，
///    INTEGER PRIMARY KEY 单列时 id.pk = 1 且其余列为 0）；
/// 2. CHECK 语义：从 `sqlite_master.sql` 建表原文解析 CHECK 表达式
///    （词法扫描提取，字符串/注释/引用标识符中的 check 字样忽略），
///    规范化（小写、去空白）后必须精确等于 `check(id=1)` —— 单一
///    `id = 1` 比较，不允许 OR/AND、其他列或其他常量放宽；
///    允许 `CHECK (id = 1)` / `CHECK(id=1)` 等空白差异，以及
///    `CONSTRAINT <名> CHECK (id = 1)` 前缀形式（约束名可引号包裹）。
///
/// 表缺失或任一约束不符合时返回 false。
pub(super) fn config_meta_constraints_match(connection: &rusqlite::Connection) -> Result<bool> {
    // 1) 主键绑定：收集全部 origin='pk' 索引的列（非 rowid 别名主键
    //    才有 sqlite_autoindex 索引）。
    let mut pk_columns = Vec::new();
    let mut found_pk_index = false;
    {
        let mut statement = connection.prepare("PRAGMA index_list('config_meta')")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, String>(3)?))
        })?;
        for row in rows {
            let (index_name, origin) = row?;
            if origin != "pk" {
                continue;
            }
            found_pk_index = true;
            let mut info = connection.prepare(&format!("PRAGMA index_info({index_name})"))?;
            let columns = info.query_map([], |row| row.get::<_, String>(2))?;
            for column in columns {
                pk_columns.push(column?);
            }
        }
    }
    if found_pk_index {
        // 有主键索引：主键必须恰好是 id 一列。
        if pk_columns.len() != 1 || pk_columns[0] != "id" {
            return Ok(false);
        }
    } else {
        // 无主键索引：INTEGER PRIMARY KEY 单列（rowid 别名）。
        // table_info.pk 是列在（复合）主键中的序号（1 起），
        // 必须是 id 单独占主键（id.pk = 1 且其余列 pk = 0）。
        let mut pk_flagged_columns = Vec::new();
        {
            let mut statement = connection.prepare("PRAGMA table_info(config_meta)")?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, i64>(5)?))
            })?;
            for row in rows {
                let (name, pk) = row?;
                if pk > 0 {
                    pk_flagged_columns.push(name);
                }
            }
        }
        if pk_flagged_columns.len() != 1 || pk_flagged_columns[0] != "id" {
            return Ok(false);
        }
    }
    // 2) CHECK 语义：解析建表 SQL 中的 CHECK 表达式并精确比对。
    let sql: Option<String> = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'config_meta'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let Some(sql) = sql else {
        return Ok(false);
    };
    let checks = extract_check_constraints(&sql);
    if checks.is_empty() {
        return Ok(false);
    }
    Ok(checks
        .iter()
        .all(|check| normalize_check(check) == "check(id=1)"))
}

/// 从建表 SQL 中提取全部 CHECK 约束子句（含 check 关键字与括号，保留原文
/// 大小写与空白）。sqlite_master.sql 保存的是建表语句原文，格式确定，
/// 用最小 SQL 词法扫描器单遍扫描：单引号字符串常量（含 `''` 转义）、
/// `--` 行注释、`/* */` 块注释、双引号/方括号/反引号引用标识符整体跳过
/// （其中的 check 字样不视为关键字，避免伪匹配），其余位置找词边界的
/// `check` 关键字，括号配对取完整表达式（允许括号嵌套，如
/// `CHECK ((id = 1))`；字符串/注释/引用标识符内的括号不参与配对）。
pub(super) fn extract_check_constraints(sql: &str) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut clauses = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        // 词法元素（字符串/注释/引用标识符）整体跳过，其中的 check 不视为关键字。
        let skipped = skip_lexical_element(bytes, index);
        if skipped != index {
            index = skipped;
            continue;
        }
        // 大小写不敏感的 check 关键字，要求前后都不是标识符字符（词边界）。
        let is_check = bytes[index].eq_ignore_ascii_case(&b'c')
            && bytes
                .get(index + 1)
                .is_some_and(|byte| byte.eq_ignore_ascii_case(&b'h'))
            && bytes
                .get(index + 2)
                .is_some_and(|byte| byte.eq_ignore_ascii_case(&b'e'))
            && bytes
                .get(index + 3)
                .is_some_and(|byte| byte.eq_ignore_ascii_case(&b'c'))
            && bytes
                .get(index + 4)
                .is_some_and(|byte| byte.eq_ignore_ascii_case(&b'k'))
            && bytes
                .get(index + 5)
                .is_none_or(|byte| !is_identifier_byte(*byte))
            && (index == 0 || !is_identifier_byte(bytes[index - 1]));
        if !is_check {
            index += 1;
            continue;
        }
        // 跳过关键字后的空白与注释，下一个非空白字符必须是 '('。
        let mut cursor = index + 5;
        loop {
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if cursor >= bytes.len() {
                break;
            }
            let skipped = skip_lexical_element(bytes, cursor);
            if skipped == cursor {
                break;
            }
            cursor = skipped;
        }
        if bytes.get(cursor) != Some(&b'(') {
            index += 1;
            continue;
        }
        // 括号配对：取到与左括号匹配的右括号为止（允许嵌套；
        // 字符串/注释/引用标识符内的括号不参与配对）。
        let mut depth = 0i32;
        let mut end = None;
        while cursor < bytes.len() {
            let skipped = skip_lexical_element(bytes, cursor);
            if skipped != cursor {
                cursor = skipped;
                continue;
            }
            match bytes[cursor] {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(cursor);
                        break;
                    }
                }
                _ => {}
            }
            cursor += 1;
        }
        if let Some(end) = end {
            clauses.push(sql[index..=end].to_string());
            index = end + 1;
        } else {
            index += 1;
        }
    }
    clauses
}

/// 标识符字节（check 关键字词边界与约束名剥离的字节版判断）。
/// 非 ASCII 字节（>= 0x80）一律视为标识符延续：UTF-8 多字节序列的每个字节
/// 都 >= 0x80，`check` 后紧跟非 ASCII 字符（如 `checké`）不是关键字边界。
pub(super) fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$') || byte >= 0x80
}

/// 若 `bytes[i..]` 以 SQL 词法元素开头，返回跳过该元素后的下标：
/// - `'...'` 单引号字符串常量（`''` 转义，未闭合按到结尾）；
/// - `-- ...` 行注释（到换行或结尾）；
/// - `/* ... */` 块注释（未闭合按到结尾）；
/// - `"..."` 双引号、`[...]` 方括号、`` `...` `` 反引号引用标识符
///   （双引号内 `""` 转义，未闭合按到结尾）。
///   否则返回 `i` 原值。调用方保证 `i < bytes.len()`。
pub(super) fn skip_lexical_element(bytes: &[u8], i: usize) -> usize {
    match bytes[i] {
        b'\'' => {
            // 单引号字符串：`''` 转义；未闭合按到结尾。
            let mut j = i + 1;
            while j < bytes.len() {
                if bytes[j] == b'\'' {
                    if bytes.get(j + 1) == Some(&b'\'') {
                        j += 2;
                    } else {
                        return j + 1;
                    }
                } else {
                    j += 1;
                }
            }
            bytes.len()
        }
        b'-' if bytes.get(i + 1) == Some(&b'-') => {
            // -- 行注释：到换行或结尾（停在换行处，由主循环继续处理）。
            let mut j = i + 2;
            while j < bytes.len() && bytes[j] != b'\n' {
                j += 1;
            }
            j
        }
        b'/' if bytes.get(i + 1) == Some(&b'*') => {
            // /* */ 块注释：未闭合按到结尾。
            let mut j = i + 2;
            while j + 1 < bytes.len() {
                if bytes[j] == b'*' && bytes[j + 1] == b'/' {
                    return j + 2;
                }
                j += 1;
            }
            bytes.len()
        }
        b'"' => {
            // 双引号引用标识符：`""` 转义；未闭合按到结尾。
            let mut j = i + 1;
            while j < bytes.len() {
                if bytes[j] == b'"' {
                    if bytes.get(j + 1) == Some(&b'"') {
                        j += 2;
                    } else {
                        return j + 1;
                    }
                } else {
                    j += 1;
                }
            }
            bytes.len()
        }
        b'[' => {
            // 方括号引用标识符：到 ] 结束；未闭合按到结尾。
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != b']' {
                j += 1;
            }
            if j < bytes.len() { j + 1 } else { bytes.len() }
        }
        b'`' => {
            // 反引号引用标识符：到 ` 结束；未闭合按到结尾。
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != b'`' {
                j += 1;
            }
            if j < bytes.len() { j + 1 } else { bytes.len() }
        }
        _ => i,
    }
}

/// 规范化 CHECK 子句并剥离可选的 `constraint <标识符>` 前缀：
/// 剥离在原文上进行（保留空白/换行，使 `--` 行注释能正确终止），
/// `CONSTRAINT` 与约束名之间、约束名与 `CHECK` 之间允许空白与 SQL 注释；
/// 约束名可为裸标识符或引号包裹形式（".." / [..] / `..`，引号内支持 `""` 转义），
/// 剥离后剩余部分小写化、去空白（如 `check(id=1)`）返回。
pub(super) fn normalize_check(clause: &str) -> String {
    let bytes = clause.as_bytes();
    // 跳过前导空白与注释，定位可能的 constraint 关键字。
    let index = skip_whitespace_and_comments(bytes, 0);
    // 大小写不敏感的 constraint 关键字，要求前后都是词边界
    // （避免把 constrainta 之类标识符误当关键字）。
    let head = &bytes[index..];
    let matches_constraint = head.len() >= 10
        && head[..10].eq_ignore_ascii_case(b"constraint")
        && head.get(10).is_none_or(|byte| !is_identifier_byte(*byte))
        && (index == 0 || !is_identifier_byte(bytes[index - 1]));
    if !matches_constraint {
        return normalize_sql(clause);
    }
    // 跳过 CONSTRAINT 与约束名之间的空白/注释。
    let mut index = skip_whitespace_and_comments(bytes, index + 10);
    // 约束名：引号包裹（".." / [..] / `..`，含 "" 转义）或裸标识符。
    index = match bytes.get(index) {
        Some(b'"') | Some(b'[') | Some(b'`') => skip_lexical_element(bytes, index),
        Some(byte) if is_identifier_byte(*byte) => {
            let mut end = index + 1;
            while end < bytes.len() && is_identifier_byte(bytes[end]) {
                end += 1;
            }
            end
        }
        _ => index,
    };
    // 跳过约束名与 CHECK 之间的空白/注释，剩余部分即为 CHECK 表达式。
    let index = skip_whitespace_and_comments(bytes, index);
    normalize_sql(&clause[index..])
}

/// 从 `bytes[i..]` 起连续跳过空白与 SQL 注释（`--` 行注释、`/* */` 块注释），
/// 返回第一个既非空白也非注释的字节下标；`i` 可以等于 `bytes.len()`。
/// 只跳过注释，不跳过引号/字符串（约束名等需要单独剥离）。
pub(super) fn skip_whitespace_and_comments(bytes: &[u8], mut i: usize) -> usize {
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            return i;
        }
        let is_comment = match bytes[i] {
            b'-' => bytes.get(i + 1) == Some(&b'-'),
            b'/' => bytes.get(i + 1) == Some(&b'*'),
            _ => false,
        };
        if !is_comment {
            return i;
        }
        i = skip_lexical_element(bytes, i);
    }
}

/// SQL 文本规范化：小写化并去掉全部空白（比较约束片段、CHECK 表达式时
/// 忽略排版差异）。
pub(super) fn normalize_sql(sql: &str) -> String {
    sql.chars()
        .filter(|ch| !ch.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}
