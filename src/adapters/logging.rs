//! 文件日志器。
//!
//! # 两个日志流
//!
//! - **主日志**（`miliastra-wonderland-music[-日期].log`）：运行期全部常规日志。
//!   面向「事后读日志的人」，一行一条事件，格式为
//!   `[月-日 时:分:秒] 级别  正文`（级别后两个空格对齐正文）。
//! - **性能日志**（`…-timing[-日期].log`，`target: "timing"`）：给脚本做性能分析用，
//!   是字段化的 `key=value`，不参与可读性重构，格式由消费脚本决定。
//!
//! 另有 `target: "performance"`（独立字段流，同属脚本消费）与
//! `target: "web_token"`（内容含完整访问令牌，只进 TUI/stderr，不落文件）。
//!
//! # 级别语义
//!
//! 分级按**事件在正常运行中出现的频繁程度**判定，不按严重程度：
//!
//! | 级别 | 频繁程度 | 典型内容 |
//! |---|---|---|
//! | `trace` | 每帧 / 每轮循环 | 逐帧匹配分数、每轮扫描分类结果 |
//! | `debug` | 每次状态流转 | 命令解析、队列增删、OCR 单次输出 |
//! | `info` | 玩家能感知 | 收到命令、点歌结果、值守切换、启停 |
//! | `warn` | 偶发 | 可恢复异常、降级、重试、兜底路径生效 |
//! | `error` | 罕见 | 本次功能最终失败，需要人工介入 |
//!
//! 完整规范见 `technical-docs/logging-style.md`。
//!
//! # 级别过滤
//!
//! 配置里的级别是**全局上限**，但第三方 crate（wgpu / naga 等）另受限：
//! 见 [target_level]。

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use log::{LevelFilter, Log, Metadata, Record, SetLoggerError};

use crate::config::LoggingConfig;
use crate::runtime::monitor::MonitorLogSink;

/// 全局文件日志器引用：init_file 设置全局 logger 后由动态附加
/// （attach_sink / set_stderr）经此引用调整 monitor sink 与 stderr 开关。
static FILE_LOGGER: OnceLock<&'static FileLogger> = OnceLock::new();

struct FileLogger {
    files: Mutex<LogFiles>,
    log_dir: PathBuf,
    rotate_daily: bool,
    retain_days: u32,
    level: LevelFilter,
    monitor: Mutex<Option<MonitorLogSink>>,
    stderr: AtomicBool,
}

struct LogFiles {
    day: i64,
    main_path: PathBuf,
    timing_path: PathBuf,
    main: File,
    timing: File,
}

impl Log for FileLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= target_level(metadata.target(), self.level)
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let now = SystemTime::now();
        let line = format!(
            "{} {}",
            format_prefix_at(record.level(), now),
            record.args()
        );
        let mut files = match self.files.lock() {
            Ok(files) => files,
            Err(_) => {
                self.write_fallback("日志文件锁已损坏，本条日志未写入文件");
                return;
            }
        };
        if let Err(error) =
            files.rotate_if_needed(&self.log_dir, self.rotate_daily, self.retain_days, now)
        {
            self.write_fallback(&format!("日志轮转失败，继续写入当前文件: {error:#}"));
        }
        if is_timing_target(record.target()) {
            let _ = files.timing.write_all(format!("{line}\n").as_bytes());
            return;
        }
        // 敏感 target（如完整 Web 访问令牌）只进 TUI/stderr，文件日志不落盘。
        if is_file_hidden_target(record.target()) {
            if let Ok(monitor) = self.monitor.lock()
                && let Some(monitor) = monitor.as_ref()
            {
                monitor.push(line.clone());
            }
            if self.stderr.load(Ordering::Relaxed) {
                let _ = std::io::stderr().write_all(format!("{line}\n").as_bytes());
            }
            return;
        }

        if !is_monitor_hidden_target(record.target())
            && let Ok(monitor) = self.monitor.lock()
            && let Some(monitor) = monitor.as_ref()
        {
            monitor.push(line.clone());
        }
        if self.stderr.load(Ordering::Relaxed) {
            let _ = std::io::stderr().write_all(format!("{line}\n").as_bytes());
        }
        let _ = files.main.write_all(format!("{line}\n").as_bytes());
    }

    fn flush(&self) {
        if let Ok(mut files) = self.files.lock() {
            let _ = files.main.flush();
            let _ = files.timing.flush();
        }
    }
}

impl FileLogger {
    /// 文件日志器自身不可用时（锁损坏、轮转失败）的兜底输出。
    ///
    /// 走到这里说明**本条日志进不了文件**：不能再用 `log::warn!`（会递归回到
    /// 同一个坏掉的文件器），只能直接写 stderr。前缀与主格式保持一致，避免
    /// 出现第二种行形状；级别固定为 WARN，因为能触发兜底的只有可降级的故障。
    fn write_fallback(&self, message: &str) {
        if self.stderr.load(Ordering::Relaxed) {
            let prefix = format_prefix_at(log::Level::Warn, SystemTime::now());
            let _ = std::io::stderr().write_all(format!("{prefix} {message}\n").as_bytes());
        }
    }
}

impl LogFiles {
    fn open(log_dir: &Path, rotate_daily: bool, now: SystemTime) -> Result<Self> {
        let day = local_day(now);
        let (main_path, timing_path) = log_paths_for_day(log_dir, day, rotate_daily)?;
        let main = open_append_file(&main_path, "日志")?;
        let timing = open_append_file(&timing_path, "性能日志")?;
        Ok(Self {
            day,
            main_path,
            timing_path,
            main,
            timing,
        })
    }

    fn rotate_if_needed(
        &mut self,
        log_dir: &Path,
        rotate_daily: bool,
        retain_days: u32,
        now: SystemTime,
    ) -> Result<()> {
        let day = local_day(now);
        if rotate_daily && day != self.day {
            let next = Self::open(log_dir, true, now)?;
            *self = next;
            cleanup_old_daily_logs(log_dir, day, retain_days)?;
        }
        Ok(())
    }
}

pub(crate) struct LogPaths {
    pub(crate) main: PathBuf,
    pub(crate) timing: PathBuf,
}

/// 日志行前缀：`[月-日 时:分:秒] 级别  正文`。
///
/// 级别右对齐到 5 字符，**前缀不含尾随空格**。
///
/// 前缀与正文之间的分隔空格由调用方统一提供（主路径 `log()` 用 `"{{}} {{}}"`，
/// 兜底路径同样在中括号后加一个空格），两条路径的正文因此落在同一列。
/// 此前前缀带尾随空格、`log()` 又补一个，实际输出三个空格；而兜底路径只补
/// 一个格式串里的空格——正是本规范要消灭的「第二种行形状」。
///
/// 不再使用方括号包住级别、也不再在正文前加冒号分隔符。
/// 不带年份：日志按日归档，文件名里已有完整日期，行内再写一遍只是噪音。
fn format_prefix_at(level: log::Level, now: SystemTime) -> String {
    format!("[{}] {level:<5}", format_timestamp(now))
}

pub(crate) fn format_time(now: SystemTime) -> String {
    format_timestamp(now)
}

/// 文件名安全的紧凑时间戳：`YYYYmmdd-HHMMSS`（本地 +08:00）。
///
/// 日志用的 `MM-DD HH:MM:SS` 含空格与冒号，Windows 文件名不接受；
/// 备份文件名还要年份才能区分不同年份的升级。
pub(crate) fn format_timestamp_compact(now: SystemTime) -> String {
    let duration = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = duration.as_secs() as i64 + 8 * 3600;
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = second_of_day / 3600;
    let minute = second_of_day % 3600 / 60;
    let second = second_of_day % 60;
    format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}")
}

fn format_timestamp(time: SystemTime) -> String {
    let duration = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = duration.as_secs() as i64 + 8 * 3600;
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400);
    let (_, month, day) = civil_from_days(days);
    let hour = second_of_day / 3600;
    let minute = second_of_day % 3600 / 60;
    let second = second_of_day % 60;
    format!("{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}")
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if month <= 2 { 1 } else { 0 };
    (year, month as u32, day as u32)
}

/// 两阶段日志第一步：仅初始化文件日志（stderr 输出开启、monitor sink 未附加）。
///
/// 启动流程在 BootstrapConfig 读取成功后立即调用，保证数据库/完整配置加载
/// 失败时错误已进入日志文件；TUI 启动完成后由 [`attach_sink`] / [`set_stderr`]
/// 动态附加 monitor sink 并关闭 stderr。
pub(crate) fn init_file(config: &LoggingConfig) -> Result<LogPaths> {
    fs::create_dir_all(&config.dir)
        .with_context(|| format!("创建日志目录「{}」", config.dir.display()))?;
    let now = SystemTime::now();
    let files = LogFiles::open(&config.dir, config.rotate_daily, now)?;
    let paths = LogPaths {
        main: files.main_path.clone(),
        timing: files.timing_path.clone(),
    };
    let cleanup_error = cleanup_old_daily_logs(&config.dir, local_day(now), config.retain_days)
        .err()
        .map(|error| format!("{error:#}"));
    let level = parse_level(&config.level);
    let logger = FileLogger {
        files: Mutex::new(files),
        log_dir: config.dir.clone(),
        rotate_daily: config.rotate_daily,
        retain_days: config.retain_days,
        level,
        monitor: Mutex::new(None),
        stderr: AtomicBool::new(true),
    };
    set_logger(logger).context("注册全局日志器（可能已被其它组件抢先注册）")?;
    log::set_max_level(level);
    if let Some(error) = cleanup_error {
        log::warn!(
            "启动时清理过期日志失败（步骤：初始化日志目录），本次保留全部旧日志，磁盘占用暂时偏高，下一次启动会重试：{error}"
        );
    }
    Ok(paths)
}

/// 两阶段日志第二步：动态附加（或替换）TUI 的 monitor 日志 sink。
/// 日志尚未初始化或全局 logger 类型不匹配时静默忽略。
pub(crate) fn attach_sink(sink: MonitorLogSink) {
    if let Some(logger) = logger_downcast()
        && let Ok(mut monitor) = logger.monitor.lock()
    {
        *monitor = Some(sink);
    }
}

/// 动态开关 stderr 输出：TUI 全屏启动后关闭，避免终端界面被日志输出干扰；
/// TUI 未启用或回退普通输出时保持开启。
pub(crate) fn set_stderr(enabled: bool) {
    if let Some(logger) = logger_downcast() {
        logger.stderr.store(enabled, Ordering::Relaxed);
    }
}

/// 取全局 logger 的 FileLogger 引用（日志未初始化时返回 None）。
fn logger_downcast() -> Option<&'static FileLogger> {
    FILE_LOGGER.get().copied()
}

fn open_append_file(path: &Path, label: &str) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("打开{label}文件「{}」", path.display()))
}

fn log_paths_for_day(log_dir: &Path, day: i64, rotate_daily: bool) -> Result<(PathBuf, PathBuf)> {
    if !rotate_daily {
        return Ok((
            log_dir.join("miliastra-wonderland-music.log"),
            log_dir.join("miliastra-wonderland-music-timing.log"),
        ));
    }
    let (year, month, date) = civil_from_days(day);
    let stamp = format!("{year:04}-{month:02}-{date:02}");
    Ok((
        log_dir.join(format!("miliastra-wonderland-music-{stamp}.log")),
        log_dir.join(format!("miliastra-wonderland-music-timing-{stamp}.log")),
    ))
}

fn cleanup_old_daily_logs(log_dir: &Path, today: i64, retain_days: u32) -> Result<()> {
    if retain_days == 0 {
        return Ok(());
    }
    let oldest_day = today - i64::from(retain_days.saturating_sub(1));
    for entry in fs::read_dir(log_dir)
        .with_context(|| format!("读取日志目录「{}」", log_dir.display()))?
    {
        let entry =
            entry.with_context(|| format!("读取日志目录「{}」中的条目", log_dir.display()))?;
        let path = entry.path();
        if !entry
            .file_type()
            .with_context(|| format!("读取文件类型「{}」", path.display()))?
            .is_file()
        {
            continue;
        }
        let Some(day) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(daily_log_day)
        else {
            continue;
        };
        if day < oldest_day {
            fs::remove_file(&path)
                .with_context(|| format!("删除过期日志「{}」", path.display()))?;
        }
    }
    Ok(())
}

fn daily_log_day(name: &str) -> Option<i64> {
    for prefix in [
        "miliastra-wonderland-music-",
        "miliastra-wonderland-music-timing-",
    ] {
        let Some(stamp) = name
            .strip_prefix(prefix)
            .and_then(|value| value.strip_suffix(".log"))
        else {
            continue;
        };
        let bytes = stamp.as_bytes();
        if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
            continue;
        }
        let year = std::str::from_utf8(&bytes[0..4])
            .ok()?
            .parse::<i64>()
            .ok()?;
        let month = std::str::from_utf8(&bytes[5..7])
            .ok()?
            .parse::<u32>()
            .ok()?;
        let day = std::str::from_utf8(&bytes[8..10])
            .ok()?
            .parse::<u32>()
            .ok()?;
        if let Some(day) = days_from_civil(year, month, day) {
            return Some(day);
        }
    }
    None
}

fn local_day(time: SystemTime) -> i64 {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
        + 8 * 3600;
    seconds.div_euclid(86_400)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month = month as i64;
    let day = day as i64;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

fn set_logger(logger: FileLogger) -> std::result::Result<(), SetLoggerError> {
    // 泄漏为 'static 供动态附加（attach_sink / set_stderr）通过 FILE_LOGGER 引用；
    // 全局 logger 生命周期与进程一致，泄漏是预期行为。重复初始化失败时
    // （logger 已设置）同样泄漏，由调用方报错退出，可接受。
    let logger: &'static FileLogger = Box::leak(Box::new(logger));
    log::set_logger(logger)?;
    let _ = FILE_LOGGER.set(logger);
    Ok(())
}

fn parse_level(value: &str) -> LevelFilter {
    match value.trim().to_ascii_lowercase().as_str() {
        "off" => LevelFilter::Off,
        "error" => LevelFilter::Error,
        "warn" | "warning" => LevelFilter::Warn,
        "debug" => LevelFilter::Debug,
        "trace" => LevelFilter::Trace,
        _ => LevelFilter::Info,
    }
}

fn target_level(target: &str, configured: LevelFilter) -> LevelFilter {
    if is_timing_target(target) || is_monitor_hidden_target(target) {
        return configured;
    }
    if target.starts_with("miliastra_wonderland_music") {
        return configured;
    }
    if target.starts_with("wgpu") || target.starts_with("naga") {
        return LevelFilter::Warn;
    }
    configured.min(LevelFilter::Warn)
}

fn is_timing_target(target: &str) -> bool {
    target == "timing"
}

fn is_monitor_hidden_target(target: &str) -> bool {
    target == "chat_scan_result"
}

/// 内容进入 TUI/stderr 但不写入文件日志的 target（用于完整敏感值）。
fn is_file_hidden_target(target: &str) -> bool {
    target == "web_token"
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 行前缀形状：`[月-日 时:分:秒] 级别`（**不含尾随空格**）。
    ///
    /// 用户明确要求不带年份（按日归档，文件名里已有完整日期）；
    /// 级别右对齐到 5 字符。前缀与正文之间的那个空格由**拼接层**提供，
    /// 这样主路径与兜底路径才能落在同一列——见下一个测试的断言。
    #[test]
    fn prefix_shape_has_no_year_and_aligns_levels() {
        // 2026-10-30 08:17:03 +08:00
        let at = UNIX_EPOCH + std::time::Duration::from_secs(1_793_319_423);
        // 4 字符级别靠 `{:<5}` 补齐，所以前缀本身可能以**对齐用**的单个空格结尾；
        // 5 字符级别（ERROR/DEBUG/TRACE）则完全没有尾随空格。
        assert_eq!(format_prefix_at(log::Level::Info, at), "[10-30 08:17:03] INFO ");
        assert_eq!(format_prefix_at(log::Level::Warn, at), "[10-30 08:17:03] WARN ");
        assert_eq!(format_prefix_at(log::Level::Error, at), "[10-30 08:17:03] ERROR");
        assert_eq!(format_prefix_at(log::Level::Debug, at), "[10-30 08:17:03] DEBUG");
        assert_eq!(format_prefix_at(log::Level::Trace, at), "[10-30 08:17:03] TRACE");
        // 真正的不变量：**级别字段固定 5 字符宽，前缀里最多只有对齐用的填充**。
        // 若再额外补一个空格（旧实现的写法），拼接后正文会比预期右移一列。
        let stamps = "[10-30 08:17:03] ".len();
        for level in [
            log::Level::Info,
            log::Level::Warn,
            log::Level::Error,
            log::Level::Debug,
            log::Level::Trace,
        ] {
            let prefix = format_prefix_at(level, at);
            assert_eq!(
                prefix.len(),
                stamps + 5,
                "前缀应恰好是「时间戳 + 5 字符级别」，不能多出分隔空格: {prefix:?}"
            );
        }
    }

    /// `FileLogger::log` 与 `write_fallback` 两条写出行必须**逐字同形**。
    ///
    /// 上一版只断言了 `format!("{prefix}正文")`，恰好漏掉 `log()` 那一层的
    /// `"{} {}"` 拼接：前缀自带尾随空格 + 拼接再加一个 = 三个空格，而兜底路径
    /// 只有两个。测试全绿、产物却是两种行形状。
    ///
    /// 这里复现两条路径**各自的拼接方式**并断言相等——这是唯一能锁住
    /// 「正文起点列一致」的断言；形状层的问题 `check-log-style.py` 抓不到。
    #[test]
    fn main_and_fallback_paths_render_identical_columns() {
        let at = UNIX_EPOCH + std::time::Duration::from_secs(1_793_319_423);
        let text = "点歌成功：晴天（QQ音乐）";
        for (level, label) in [
            (log::Level::Info, "INFO"),
            (log::Level::Warn, "WARN"),
            (log::Level::Error, "ERROR"),
            (log::Level::Debug, "DEBUG"),
            (log::Level::Trace, "TRACE"),
        ] {
            let prefix = format_prefix_at(level, at);
            // FileLogger::log: format!("{} {}", prefix, record.args())
            let main = format!("{} {}", prefix, text);
            // write_fallback: format!("{prefix} {message}")
            let fallback = format!("{prefix} {text}");
            assert_eq!(
                main, fallback,
                "主路径与兜底路径的正文列不一致: {main:?} vs {fallback:?}"
            );
            // 正文应恰好从「时间戳 + 5 宽级别 + 一个空格」之后开始。
            let expected = format!("[10-30 08:17:03] {label:<5} {text}");
            assert_eq!(main, expected, "正文起点列不是预期的两空格");
        }
    }

    /// 前缀里不得残留旧格式的分隔符：方括号只包住时间戳，级别后面不带冒号。
    #[test]
    fn prefix_carries_no_legacy_separators() {
        let at = UNIX_EPOCH + std::time::Duration::from_secs(1_793_319_423);
        for (level, label) in [
            (log::Level::Info, "INFO"),
            (log::Level::Warn, "WARN"),
            (log::Level::Error, "ERROR"),
            (log::Level::Debug, "DEBUG"),
            (log::Level::Trace, "TRACE"),
        ] {
            let prefix = format_prefix_at(level, at);
            assert!(
                !prefix.contains("] :") && !prefix.contains("]["),
                "前缀残留旧分隔符: {prefix:?}"
            );
            assert_eq!(prefix.matches('[').count(), 1, "只应有一个方括号开始: {prefix:?}");
            assert_eq!(prefix.matches(']').count(), 1, "只应有一个方括号结束: {prefix:?}");
            // 拼上正文后，级别与正文之间始终是两个空格（级别不足 5 字符时补齐）。
            // 注意：这个空格由**拼接层**提供，前缀本身不带尾随空格。
            let line = format!("{prefix} 点歌成功：晴天（QQ音乐）");
            assert_eq!(
                line,
                format!("[10-30 08:17:03] {label:<5} 点歌成功：晴天（QQ音乐）")
            );
        }
    }

    /// 兜底输出与主格式同形：不能出现第二种行形状（旧实现硬编码 `[WARN ] `）。
    #[test]
    fn fallback_prefix_matches_the_main_format() {
        let prefix = format_prefix_at(log::Level::Warn, SystemTime::now());
        assert!(prefix.contains("] WARN"), "兜底前缀应含对齐后的 WARN: {prefix:?}");
        assert!(!prefix.contains("[WARN ]"), "不得残留旧硬编码前缀: {prefix:?}");
        // 兜底与主路径都用「前缀 + 一个空格 + 正文」，正文列必须相同。
        let text = "日志文件锁已损坏，本条日志未写入文件";
        assert_eq!(
            format!("{prefix} {text}"),
            format!("{} {}", prefix, text),
            "兜底路径与主路径的拼接方式必须一致"
        );
    }

    /// 第三方 crate 的日志固定在 Warn 一档（wgpu/naga 的每帧噪声不再进文件），
    /// 本程序 target 与两个脚本消费的 target 跟随配置。
    ///
    /// 注意 `wgpu`/`naga` 走的是**硬编码 Warn**，不是 `configured.min(Warn)`：
    /// 即使配置调到 error，这两个 crate 的 warn 仍会输出。这是既有取舍——
    /// 显卡初始化只在启动阶段发生，warn 量有限；保留不动。
    #[test]
    fn third_party_targets_stay_capped_at_warn() {
        let configured = LevelFilter::Trace;
        assert_eq!(target_level("wgpu_core", configured), LevelFilter::Warn);
        assert_eq!(target_level("naga", configured), LevelFilter::Warn);
        assert_eq!(target_level("some_other_crate", configured), LevelFilter::Warn);
        assert_eq!(
            target_level("miliastra_wonderland_music::runtime::monitor", configured),
            LevelFilter::Trace,
            "本程序 target 不应被降级"
        );
        assert_eq!(
            target_level("timing", configured),
            LevelFilter::Trace,
            "性能日志跟随配置，它由脚本消费"
        );
        assert_eq!(
            target_level("web_token", configured),
            LevelFilter::Warn,
            "web_token 只用于报告访问令牌异常，本身是 warn 级事件；它走的是「不落文件」分支而不是「跟随配置」分支"
        );
    }

    #[test]
    fn daily_log_names_parse_both_log_streams() {
        assert_eq!(
            daily_log_day("miliastra-wonderland-music-2026-07-11.log"),
            days_from_civil(2026, 7, 11)
        );
        assert_eq!(
            daily_log_day("miliastra-wonderland-music-timing-2026-07-11.log"),
            days_from_civil(2026, 7, 11)
        );
        assert_eq!(
            daily_log_day("miliastra-wonderland-music-2026-02-30.log"),
            None
        );
        assert_eq!(daily_log_day("unrelated.log"), None);
    }
}
