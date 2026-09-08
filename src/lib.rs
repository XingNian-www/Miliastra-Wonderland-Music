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

use std::sync::atomic::{AtomicBool, Ordering};

pub mod observation;

pub mod runtime;

/// 看门狗子进程用于正常配置重载的退出码。
pub const CONFIG_RELOAD_EXIT_CODE: u8 = 75;

/// 请求替代进程执行启动自动化的配置重载退出码。
pub const CONFIG_RELOAD_WITH_STARTUP_EXIT_CODE: u8 = 77;

/// 仅由配置重载后启动的子进程设置的一次性标记。
pub const CONFIG_RELOAD_CHILD_ENV: &str = "MILIASTRA_CONFIG_RELOAD_CHILD";

/// 启动字段发生变化时由看门狗传递给替代进程的一次性标记。
pub const CONFIG_RELOAD_RUN_STARTUP_ENV: &str = "MILIASTRA_CONFIG_RELOAD_RUN_STARTUP";

/// 替代进程在达到就绪点前失败时使用的退出码。
pub const CONFIG_RELOAD_STARTUP_FAILURE_EXIT_CODE: u8 = 76;

/// 看门狗用于观察替代进程就绪状态的子进程标记路径。
pub const CONFIG_RELOAD_READY_FILE_ENV: &str = "MILIASTRA_CONFIG_RELOAD_READY_FILE";

/// 替代子进程就绪时原子写入的固定标记内容。
pub const CONFIG_RELOAD_READY_MARKER: &[u8] = b"miliastra-config-reload-ready-v1\n";

static CONFIG_RELOAD_CHILD_READY: AtomicBool = AtomicBool::new(false);

/// HTTP、工作线程和扫描运行时可用后，将替代子进程标记为就绪。
///
/// 进程内标志决定子进程退出码。标记文件让看门狗即使在子进程随后崩溃或未返回 `main` 就终止时，仍能保留热重载交接信息。
pub(crate) fn mark_config_reload_child_ready() -> anyhow::Result<bool> {
    if CONFIG_RELOAD_CHILD_READY.load(Ordering::SeqCst) {
        return Ok(false);
    }
    if let Some(path) = std::env::var_os(CONFIG_RELOAD_READY_FILE_ENV) {
        let path = std::path::PathBuf::from(path);
        adapters::file_store::write_atomic(
            &path,
            CONFIG_RELOAD_READY_MARKER,
            "配置重载 ready 标记",
        )?;
    }
    CONFIG_RELOAD_CHILD_READY.store(true, Ordering::SeqCst);
    Ok(true)
}

/// 当前子进程在返回错误前是否已达到就绪点。
pub fn config_reload_child_ready() -> bool {
    CONFIG_RELOAD_CHILD_READY.load(Ordering::SeqCst)
}

/// 应用运行时完整退出后的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    Stopped,
    Reload,
    ReloadWithStartup,
}

impl RunOutcome {
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Stopped => 0,
            Self::Reload => CONFIG_RELOAD_EXIT_CODE,
            Self::ReloadWithStartup => CONFIG_RELOAD_WITH_STARTUP_EXIT_CODE,
        }
    }
}

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
pub fn run(config_path: &std::path::Path) -> anyhow::Result<RunOutcome> {
    // 发布布局：EXE 同目录只保留 config.yaml 与主程序，
    // ffmpeg/MNN 等动态库统一放在 deps/dll/，启动时加入 DLL 搜索路径。
    add_dependency_dll_directory(config_path);
    adapters::windows::dpi::set_process_dpi_awareness();
    composition::run(config_path)
}

/// 将发布目录的 `deps/dll/` 加入进程 DLL 搜索路径（SetDllDirectoryW），
/// 使 ffmpeg、MNN 等动态库从依赖文件夹加载。
#[cfg(target_os = "windows")]
fn add_dependency_dll_directory(config_path: &std::path::Path) {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::System::LibraryLoader::SetDllDirectoryW;
    use windows::core::PCWSTR;
    let Some(executable_root) = config_path.parent() else {
        return;
    };
    let dll_directory = executable_root.join("deps").join("dll");
    if !dll_directory.is_dir() {
        return;
    }
    let wide = dll_directory
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<u16>>();
    let _ = unsafe { SetDllDirectoryW(PCWSTR(wide.as_ptr())) };
}

#[cfg(target_os = "windows")]
pub fn watchdog_restart_ms(config_path: &std::path::Path) -> anyhow::Result<u64> {
    use anyhow::Context;

    let executable_root = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("配置路径缺少父目录: {}", config_path.display()))?;
    // 与主程序启动同链路：最小引导配置 → 统一 SQLite 数据库 → 完整业务配置。
    // 数据库损坏时这里返回错误会导致 watchdog 父进程退出，错误信息必须清楚。
    let bootstrap = config::BootstrapConfig::load(config_path, executable_root)
        .with_context(|| format!("读取启动配置失败: {}", config_path.display()))?;
    let store =
        config::ConfigStore::open(&bootstrap.database_path, executable_root, bootstrap.clone())
            .with_context(|| {
                format!(
                    "打开统一配置数据库失败: {}",
                    bootstrap.database_path.display()
                )
            })?;
    let config = store
        .load_full()
        .with_context(|| "从统一配置数据库加载完整配置失败")?;
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

    #[test]
    fn run_outcome_reserves_a_dedicated_reload_exit_code() {
        assert_eq!(RunOutcome::Stopped.exit_code(), 0);
        assert_eq!(RunOutcome::Reload.exit_code(), CONFIG_RELOAD_EXIT_CODE);
        assert_eq!(
            RunOutcome::ReloadWithStartup.exit_code(),
            CONFIG_RELOAD_WITH_STARTUP_EXIT_CODE
        );
        assert_ne!(RunOutcome::Reload.exit_code(), 0);
        assert_ne!(
            RunOutcome::ReloadWithStartup.exit_code(),
            RunOutcome::Reload.exit_code()
        );
    }
}
