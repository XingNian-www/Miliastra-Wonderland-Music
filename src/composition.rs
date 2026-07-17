use std::io::IsTerminal;
use std::path::Path;

use anyhow::Result;

use crate::adapters::logging;
use crate::config::AppConfig;
use crate::interfaces::tui::TuiHandle;
use crate::runtime::monitor::MonitorShared;

pub(crate) mod application;
use application::{ApplicationRuntime, ResolvedApplicationConfig};

pub(crate) fn run(config_path: &Path) -> Result<()> {
    let config = AppConfig::load(config_path)?;
    let config = ResolvedApplicationConfig::resolve(config)?;
    let app_config = config.app();
    let monitor = MonitorShared::new(app_config.tui.log_lines);
    let tui_handle = if app_config.tui.enabled && std::io::stdout().is_terminal() {
        match TuiHandle::start(&app_config.tui, monitor.clone()) {
            Ok(handle) => Some(handle),
            Err(error) => {
                eprintln!("TUI 启动失败，回退普通日志输出: {error:#}");
                None
            }
        }
    } else if app_config.tui.enabled {
        eprintln!("检测到非交互终端，已关闭 TUI");
        None
    } else {
        None
    };
    let log_paths = logging::init(
        &app_config.logging,
        Some(monitor.log_sink()),
        tui_handle.is_none(),
    )?;
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
        "FeelUOwn: {}:{}",
        app_config.feeluown.host,
        app_config.feeluown.port
    );

    let mut app = ApplicationRuntime::new(config, monitor.clone())?;
    let result = app.run();
    drop(tui_handle);
    monitor.shutdown();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_configuration_resolves_before_runtime_construction() {
        let config = AppConfig::load(Path::new("config.yaml")).expect("load bundled config");

        let resolved = ResolvedApplicationConfig::resolve(config)
            .expect("resolve all module configuration before runtime construction");

        assert!(!resolved.app().window.target_process.trim().is_empty());
    }
}
