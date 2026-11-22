//! 真实截图 / 技术文档语料的可用性判定。
//!
//! `tests/` 与 `technical-docs/` 在 `src/` 之外、由 `.gitignore` 的 `/*` 白名单挡住，
//! **不入库**：里面放的是真实玩家画面截图、真帧证据画廊和分支本地配置。
//! 干净检出（fresh clone / CI 无缓存）时它们不存在，因此直接
//! `image::open("tests/fixtures/ui/...")` 的用例必然 panic——那与产品代码是否回归无关。
//!
//! 本模块提供：
//!
//! 1. [`corpus_available`]：语料是否真的可用；
//! 2. [`skip_without_corpus`]：语料缺失时记一条可读原因并让调用方 `return`。
//!
//! 判定顺序：
//!
//! | 来源 | 结论 |
//! |---|---|
//! | `MILIASTRA_TEST_CORPUS=1` | 强制**认为有语料**（缺失即真红——CI 明确断言用） |
//! | `MILIASTRA_TEST_CORPUS=0` | 强制**认为无语料**（跳过；用来复现干净检出） |
//! | 两者皆无 | 目录探测：`tests/fixtures/ui` 或 `technical-docs` 存在即认为有 |
//!
//! 为什么不用 `#[ignore]`：`#[ignore]` 在测试输出里只显示 `ignored`，CI 无法区分
//! 「作者主动忽略」与「语料根本不存在」，而且被忽略的用例永远不会因为语料回归而变红。
//! 运行时跳过会把原因打到 stderr，并且 `MILIASTRA_TEST_CORPUS=1` 时可以强制它们真跑。
//!
//! 跳过原因**直接写 `stderr` 句柄**而不是走 `eprintln!`：libtest 的输出捕获只拦
//! `print!`/`eprintln!` 这类宏，直接写句柄能穿透捕获，因此默认 `cargo test` 的输出里
//! 就能看到每一条跳过，不必额外加 `--nocapture` / `--show-output`。
//! 同时把跳过清单落到 `target/test-corpus-skips.txt`（`MILIASTRA_TEST_CORPUS_SKIP_LOG`
//! 可覆盖路径），供 CI 断言「到底跳了多少条」。

use std::io::Write;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

/// 强制开关的环境变量名。
pub(crate) const CORPUS_ENV: &str = "MILIASTRA_TEST_CORPUS";

/// 跳过清单文件的落盘路径覆盖。
const SKIP_LOG_ENV: &str = "MILIASTRA_TEST_CORPUS_SKIP_LOG";
/// 跳过清单默认落盘路径（在 cargo 的 target 目录内，天然被 git 忽略）。
const SKIP_LOG_PATH: &str = "target/test-corpus-skips.txt";

/// 语料根目一：真实截图与分支本地配置。
const FIXTURE_ROOT: &str = "tests/fixtures/ui";
/// 语料根目二：真帧证据画廊与技术文档。
const DOCS_ROOT: &str = "technical-docs";

static SKIPPED: AtomicUsize = AtomicUsize::new(0);

/// 真实截图/技术文档语料是否可用。
///
/// 只探测目录存在性，不读文件内容，因此不会因为某一张图缺失就整片跳过；
/// 单张图缺失仍应让用例真红（那是语料损坏，不是干净检出）。
pub(crate) fn corpus_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| match std::env::var(CORPUS_ENV).as_deref() {
        Ok("1") => true,
        Ok("0") => false,
        // 其它取值（含空串、拼错）按未设置处理，避免把拼写错误静默当成"关闭"。
        _ => Path::new(FIXTURE_ROOT).is_dir() || Path::new(DOCS_ROOT).exists(),
    })
}

/// 本次测试进程累计跳过的用例数（供报告与 CI 断言）。
pub(crate) fn skipped_count() -> usize {
    SKIPPED.load(Ordering::Relaxed)
}

/// 语料缺失时跳过用例并记一条可读原因；返回 `true` 表示调用方应立即 `return`。
///
/// `case` 用测试函数名，跳过原因里带上它，便于在长日志里定位是哪条用例被跳过。
pub(crate) fn skip_without_corpus(case: &str) -> bool {
    if corpus_available() {
        return false;
    }
    let index = SKIPPED.fetch_add(1, Ordering::Relaxed) + 1;
    // 第一条打印完整原因，后续保持单行，避免 100+ 条重复把日志刷掉。
    let line = if index == 1 {
        format!(
            "corpus-skip #{index} {case}：真实截图/技术文档语料不可用\
             （{FIXTURE_ROOT} 与 {DOCS_ROOT} 均不存在；这两个目录不入库，干净检出时必然缺失）。\
             本地/CI 备好语料后自动恢复全量；设 {CORPUS_ENV}=1 可强制要求语料存在。"
        )
    } else {
        format!("corpus-skip #{index} {case}")
    };
    // 直写句柄：绕过 libtest 的输出捕获，默认 cargo test 输出里就能看到。
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "{line}");
    let _ = stderr.flush();
    record_skip(&line);
    true
}

/// 追加一行到跳过清单；写不进去也不影响用例结论，因此只忽略错误。
fn record_skip(line: &str) {
    let path = std::env::var_os(SKIP_LOG_ENV)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(SKIP_LOG_PATH));
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 语料判定只看目录与环境变量，不依赖当前工作目录里有真的语料——
    /// 这条用例本身在干净检出下也必须能跑。
    #[test]
    fn corpus_probe_matches_the_filesystem_and_the_override() {
        let detected = Path::new(FIXTURE_ROOT).is_dir() || Path::new(DOCS_ROOT).exists();
        let available = corpus_available();
        match std::env::var(CORPUS_ENV).as_deref() {
            Ok("1") => assert!(available, "{CORPUS_ENV}=1 必须强制认为有语料"),
            Ok("0") => assert!(!available, "{CORPUS_ENV}=0 必须强制认为无语料"),
            _ => assert_eq!(available, detected, "未设置覆盖时必须按目录探测"),
        }
    }

    /// 跳过助手必须与判定同源：有语料时不跳过，无语料时才跳过。
    #[test]
    fn skip_helper_is_the_inverse_of_availability() {
        assert_eq!(
            skip_without_corpus("corpus_helper_self_check"),
            !corpus_available()
        );
    }
}
