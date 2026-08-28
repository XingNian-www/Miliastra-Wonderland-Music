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
    fn write_fallback(&self, message: &str) {
        if self.stderr.load(Ordering::Relaxed) {
            let _ = std::io::stderr().write_all(format!("[WARN ] : {message}\n").as_bytes());
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

fn format_prefix_at(level: log::Level, now: SystemTime) -> String {
    format!("[{}][{level:<5}] :", format_timestamp(now))
}

pub(crate) fn format_time(now: SystemTime) -> String {
    format_timestamp(now)
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
        .with_context(|| format!("create log directory {}", config.dir.display()))?;
    let now = SystemTime::now();
    let files = LogFiles::open(&config.dir, config.rotate_daily, now)?;
    let paths = LogPaths {
        main: files.main_path.clone(),
        timing: files.timing_path.clone(),
    };
    let cleanup_warning = cleanup_old_daily_logs(&config.dir, local_day(now), config.retain_days)
        .err()
        .map(|error| format!("清理过期日志失败，后续运行会重试: {error:#}"));
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
    set_logger(logger).context("initialize logger")?;
    log::set_max_level(level);
    if let Some(warning) = cleanup_warning {
        log::warn!("{warning}");
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
        .with_context(|| format!("open {label} file {}", path.display()))
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
        .with_context(|| format!("read log directory {}", log_dir.display()))?
    {
        let entry = entry.with_context(|| format!("read entry in {}", log_dir.display()))?;
        let path = entry.path();
        if !entry
            .file_type()
            .with_context(|| format!("read file type {}", path.display()))?
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
                .with_context(|| format!("remove expired log {}", path.display()))?;
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
