use std::io::IsTerminal;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::adapters::logging;
use crate::config::{BootstrapConfig, ConfigStore};
use crate::interfaces::tui::TuiHandle;
use crate::runtime::monitor::MonitorShared;

pub(crate) mod application;
use application::{ApplicationRuntime, ResolvedApplicationConfig};

pub(crate) fn run(config_path: &Path) -> Result<()> {
    let executable_root = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("配置路径缺少 EXE 根目录: {}", config_path.display()))?;
    // 启动流程：最小引导配置（database_path/http/logging）→ 文件日志（两阶段第一步）
    // → 统一 SQLite 数据库 → 完整业务配置。http/logging 与 state.playback_state_path
    // 已由引导配置注入，相对路径已按 executable_root 解析，无需再调用 load_from_root。
    let bootstrap = BootstrapConfig::load(config_path, executable_root)
        .with_context(|| format!("读取启动配置失败: {}", config_path.display()))?;
    // 两阶段日志初始化：bootstrap 成功后立即初始化文件日志（stderr 输出开启），
    // 保证数据库/完整配置加载失败时错误已进入日志文件；monitor sink 与 stderr
    // 开关在 TUI 启动后通过 logging::attach_sink / logging::set_stderr 动态调整。
    // logging.dir 与 AppConfig::resolve_runtime_paths 对 logging.dir 的解析语义一致。
    let mut logging_config = bootstrap.logging.clone();
    if !logging_config.dir.as_os_str().is_empty() && logging_config.dir.is_relative() {
        logging_config.dir = executable_root.join(&logging_config.dir);
    }
    let log_paths = logging::init_file(&logging_config).with_context(|| "初始化日志失败")?;
    let store = ConfigStore::open(&bootstrap.database_path, executable_root, bootstrap.clone())
        .map_err(|error| {
            log::error!("打开统一配置数据库失败: {error:#}");
            error.context(format!(
                "打开统一配置数据库失败: {}",
                bootstrap.database_path.display()
            ))
        })?;
    let app_config = store.load_full().map_err(|error| {
        log::error!("从统一配置数据库加载完整配置失败: {error:#}");
        error.context("从统一配置数据库加载完整配置失败")
    })?;
    let config = ResolvedApplicationConfig::resolve(app_config)?;
    let app_config = config.app();
    let monitor = MonitorShared::new(app_config.tui.log_lines);
    let tui_handle = if app_config.tui.enabled && std::io::stdout().is_terminal() {
        match TuiHandle::start(&app_config.tui, monitor.clone()) {
            Ok(handle) => Some(handle),
            Err(error) => {
                log::warn!("TUI 启动失败，回退普通日志输出: {error:#}");
                None
            }
        }
    } else if app_config.tui.enabled {
        log::warn!("检测到非交互终端，已关闭 TUI");
        None
    } else {
        None
    };
    // 两阶段日志第二步：TUI 启动完成后附加 monitor sink；
    // TUI 全屏时关闭 stderr（避免终端界面被日志输出干扰），未启用时保持开启。
    logging::attach_sink(monitor.log_sink());
    logging::set_stderr(tui_handle.is_none());
    log::info!("日志文件: {}", log_paths.main.display());
    log::info!("性能日志文件: {}", log_paths.timing.display());
    log::info!("配置文件: {}", config_path.display());
    log::info!(
        "HTTP/Web 面板: {}:{} enabled={}",
        app_config.http.host,
        app_config.http.port,
        app_config.http.enabled
    );
    log::info!(
        "原生播放器: credentials={} helper={}",
        app_config.playback.credential_directory.display(),
        app_config.playback.login_helper_executable.display()
    );

    let mut app = ApplicationRuntime::new(
        config,
        monitor.clone(),
        Arc::new(std::sync::Mutex::new(store)),
    )?;
    let result = app.run();
    drop(tui_handle);
    monitor.shutdown();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    #[test]
    fn bundled_configuration_resolves_before_runtime_construction() {
        // 完整配置模板（tests/fixtures/config.full.yaml）作为 AppConfig 完整
        // YAML 的测试源；仓库根 config.yaml 已是最小启动配置（仅 bootstrap 字段）。
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml"))
            .expect("load bundled config");

        let resolved = ResolvedApplicationConfig::resolve(config)
            .expect("resolve all module configuration before runtime construction");

        assert!(!resolved.app().window.target_process.trim().is_empty());
    }
}
