use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use log::{LevelFilter, Log, Metadata, Record, SetLoggerError};

use super::monitor::MonitorLogSink;

struct FileLogger {
    file: Mutex<File>,
    timing_file: Mutex<File>,
    level: LevelFilter,
    monitor: Option<MonitorLogSink>,
    stderr: bool,
}

impl Log for FileLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= target_level(metadata.target(), self.level)
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let line = format!("{} {}", format_prefix(record.level()), record.args());
        if is_timing_target(record.target()) {
            if let Ok(mut file) = self.timing_file.lock() {
                let _ = file.write_all(format!("{line}\n").as_bytes());
            }
            return;
        }

        if let Some(monitor) = &self.monitor {
            monitor.push(line.clone());
        }
        if self.stderr {
            let _ = std::io::stderr().write_all(format!("{line}\n").as_bytes());
        }
        if let Ok(mut file) = self.file.lock() {
            let _ = file.write_all(format!("{line}\n").as_bytes());
        }
    }

    fn flush(&self) {
        if let Ok(mut file) = self.file.lock() {
            let _ = file.flush();
        }
        if let Ok(mut file) = self.timing_file.lock() {
            let _ = file.flush();
        }
    }
}

pub(super) struct LogPaths {
    pub(super) main: PathBuf,
    pub(super) timing: PathBuf,
}

pub(super) fn format_prefix(level: log::Level) -> String {
    format!("[{}][{level:<5}] :", format_timestamp(SystemTime::now()))
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

pub fn init(
    log_dir: &Path,
    level: &str,
    monitor: Option<MonitorLogSink>,
    stderr: bool,
) -> Result<LogPaths> {
    fs::create_dir_all(log_dir)
        .with_context(|| format!("create log directory {}", log_dir.display()))?;
    let path = log_dir.join("miliastra-wonderland-music.log");
    let timing_path = log_dir.join("miliastra-wonderland-music-timing.log");
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open log file {}", path.display()))?;
    let timing_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&timing_path)
        .with_context(|| format!("open timing log file {}", timing_path.display()))?;
    let level = parse_level(level);
    let logger = FileLogger {
        file: Mutex::new(file),
        timing_file: Mutex::new(timing_file),
        level,
        monitor,
        stderr,
    };
    set_logger(logger).context("initialize logger")?;
    log::set_max_level(level);
    Ok(LogPaths {
        main: path,
        timing: timing_path,
    })
}

fn set_logger(logger: FileLogger) -> std::result::Result<(), SetLoggerError> {
    log::set_boxed_logger(Box::new(logger))
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
    if is_timing_target(target) {
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
