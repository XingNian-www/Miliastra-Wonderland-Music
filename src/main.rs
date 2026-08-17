#[cfg(not(target_os = "windows"))]
fn main() {
    compile_error!("miliastra-wonderland-music only supports Windows.");
}

#[cfg(target_os = "windows")]
fn is_config_reload_exit(code: Option<i32>) -> bool {
    code == Some(i32::from(
        miliastra_wonderland_music::CONFIG_RELOAD_EXIT_CODE,
    ))
}

#[cfg(target_os = "windows")]
fn main() -> anyhow::Result<std::process::ExitCode> {
    use std::process::Command;
    use std::thread::sleep;
    use std::time::Duration;

    use anyhow::Context;
    use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::core::PCWSTR;

    let current_exe = std::env::current_exe().context("定位主程序 EXE 失败")?;
    let default_config_path = miliastra_wonderland_music::default_config_path(&current_exe)?;
    let is_watchdog_child = std::env::var_os("MILIASTRA_WATCHDOG_CHILD").is_some();
    let config_path = if is_watchdog_child {
        std::env::var_os("MILIASTRA_CONFIG_PATH")
            .map(std::path::PathBuf::from)
            .unwrap_or(default_config_path)
    } else {
        default_config_path
    };
    if is_watchdog_child {
        let outcome = miliastra_wonderland_music::run(&config_path)?;
        return Ok(std::process::ExitCode::from(outcome.exit_code()));
    }

    // 单实例互斥(仅看门狗父进程持有):避免重复启动导致全局热键
    // 注册冲突(ERROR_HOTKEY_ALREADY_REGISTERED)与配置库并发打开。
    let singleton = "Local\\miliastra-wonderland-music"
        .encode_utf16()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let _singleton_mutex = match unsafe { CreateMutexW(None, false, PCWSTR(singleton.as_ptr())) } {
        Ok(handle) => {
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                eprintln!("Miliastra 已有实例在运行,请先退出旧实例");
                return Ok(std::process::ExitCode::SUCCESS);
            }
            Some(handle)
        }
        Err(_) => None,
    };

    loop {
        let mut child = Command::new(&current_exe)
            .env("MILIASTRA_WATCHDOG_CHILD", "1")
            .env("MILIASTRA_CONFIG_PATH", &config_path)
            .spawn()
            .with_context(|| format!("启动监听子进程失败: {}", current_exe.display()))?;
        let status = child.wait().context("等待监听子进程退出")?;
        if status.success() {
            return Ok(std::process::ExitCode::SUCCESS);
        }
        if is_config_reload_exit(status.code()) {
            eprintln!("监听子进程已完成配置重载关停，立即重新启动");
            continue;
        }

        let restart_ms = miliastra_wonderland_music::watchdog_restart_ms(&config_path)?;
        eprintln!("监听子进程异常退出: status={status}，{restart_ms}ms 后重启");
        sleep(Duration::from_millis(restart_ms));
    }
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::is_config_reload_exit;

    #[test]
    fn watchdog_only_treats_the_reserved_code_as_a_configuration_reload() {
        assert!(is_config_reload_exit(Some(75)));
        assert!(!is_config_reload_exit(Some(0)));
        assert!(!is_config_reload_exit(Some(1)));
        assert!(!is_config_reload_exit(None));
    }
}
