#[cfg(target_os = "windows")]
mod adapters;
#[cfg(target_os = "windows")]
mod composition;
#[cfg(target_os = "windows")]
mod config;
#[cfg(target_os = "windows")]
mod features;
#[cfg(target_os = "windows")]
mod interfaces;
#[cfg(target_os = "windows")]
mod privacy;
#[cfg(target_os = "windows")]
mod text;
#[cfg(target_os = "windows")]
mod ui;

pub mod observation;

pub mod runtime;

#[cfg(target_os = "windows")]
pub fn run(config_path: &std::path::Path) -> anyhow::Result<()> {
    adapters::windows::dpi::set_process_dpi_awareness();
    composition::run(config_path)
}

#[cfg(target_os = "windows")]
pub fn watchdog_restart_ms(config_path: &std::path::Path) -> anyhow::Result<u64> {
    let config = config::AppConfig::load(config_path)?;
    config.validate()?;
    Ok(config.timing.watchdog_restart_ms)
}
