mod adapters;
#[cfg(target_os = "windows")]
mod composition;
mod config;
mod features;
mod interfaces;
mod privacy;
#[cfg(test)]
mod test_support;
mod text;
mod ui;

pub mod observation;

pub mod runtime;

/// 根据主程序路径计算默认配置路径，不读取进程环境，便于测试。
pub fn default_config_path(
    executable_path: &std::path::Path,
) -> anyhow::Result<std::path::PathBuf> {
    let executable_root = executable_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("主程序路径缺少父目录: {}", executable_path.display()))?;
    Ok(executable_root.join("config.yaml"))
}

#[cfg(target_os = "windows")]
pub fn run(config_path: &std::path::Path) -> anyhow::Result<()> {
    adapters::windows::dpi::set_process_dpi_awareness();
    composition::run(config_path)
}

#[cfg(target_os = "windows")]
pub fn watchdog_restart_ms(config_path: &std::path::Path) -> anyhow::Result<u64> {
    let executable_root = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("配置路径缺少父目录: {}", config_path.display()))?;
    let config = config::AppConfig::load_from_root(config_path, executable_root)?;
    config.validate()?;
    Ok(config.timing.watchdog_restart_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn default_configuration_is_next_to_the_supplied_executable() {
        let path = default_config_path(Path::new(r"C:\发布目录\主程序.exe"))
            .expect("应能计算默认配置路径");

        assert_eq!(path, PathBuf::from(r"C:\发布目录\config.yaml"));
    }

    #[test]
    fn default_configuration_rejects_an_executable_without_a_parent() {
        let error = default_config_path(Path::new("")).expect_err("缺少父目录的主程序路径必须报错");

        assert!(error.to_string().contains("主程序路径缺少父目录"));
    }
}
