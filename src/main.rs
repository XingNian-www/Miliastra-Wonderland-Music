#[cfg(not(target_os = "windows"))]
fn main() {
    compile_error!("miliastra-wonderland-music only supports Windows.");
}

#[cfg(target_os = "windows")]
fn is_config_reload_exit(code: Option<i32>) -> bool {
    matches!(
        code,
        Some(value)
            if value == i32::from(miliastra_wonderland_music::CONFIG_RELOAD_EXIT_CODE)
                || value == i32::from(
                    miliastra_wonderland_music::CONFIG_RELOAD_WITH_STARTUP_EXIT_CODE,
                )
    )
}

#[cfg(target_os = "windows")]
fn config_reload_exit_requires_startup(code: Option<i32>) -> bool {
    code == Some(i32::from(
        miliastra_wonderland_music::CONFIG_RELOAD_WITH_STARTUP_EXIT_CODE,
    ))
}

#[cfg(target_os = "windows")]
const DEFAULT_WATCHDOG_RESTART_MS: u64 = 2000;

#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct WatchdogChildHandoff {
    config_reload: bool,
    run_startup: bool,
}

#[cfg(target_os = "windows")]
impl WatchdogChildHandoff {
    fn after_exit(previous: Self, code: Option<i32>, replacement_ready: bool) -> Self {
        let preserve_pre_ready_handoff = previous.config_reload && !replacement_ready;
        Self {
            config_reload: is_config_reload_exit(code) || preserve_pre_ready_handoff,
            run_startup: config_reload_exit_requires_startup(code)
                || (preserve_pre_ready_handoff && previous.run_startup),
        }
    }
}

#[cfg(target_os = "windows")]
fn configure_watchdog_child(
    command: &mut std::process::Command,
    config_path: &std::path::Path,
    handoff: WatchdogChildHandoff,
    ready_file: Option<&std::path::Path>,
) {
    command
        .env("MILIASTRA_WATCHDOG_CHILD", "1")
        .env("MILIASTRA_CONFIG_PATH", config_path);
    if handoff.config_reload {
        command.env(miliastra_wonderland_music::CONFIG_RELOAD_CHILD_ENV, "1");
        if let Some(ready_file) = ready_file {
            command.env(
                miliastra_wonderland_music::CONFIG_RELOAD_READY_FILE_ENV,
                ready_file,
            );
        }
        if handoff.run_startup {
            command.env(
                miliastra_wonderland_music::CONFIG_RELOAD_RUN_STARTUP_ENV,
                "1",
            );
        } else {
            command.env_remove(miliastra_wonderland_music::CONFIG_RELOAD_RUN_STARTUP_ENV);
        }
    } else {
        command.env_remove(miliastra_wonderland_music::CONFIG_RELOAD_CHILD_ENV);
        command.env_remove(miliastra_wonderland_music::CONFIG_RELOAD_READY_FILE_ENV);
        command.env_remove(miliastra_wonderland_music::CONFIG_RELOAD_RUN_STARTUP_ENV);
    }
}

#[cfg(target_os = "windows")]
fn replacement_ready_file() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "miliastra-reload-ready-{}-{}.marker",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ))
}

#[cfg(target_os = "windows")]
fn replacement_is_ready(path: &std::path::Path) -> bool {
    std::fs::read(path).is_ok_and(|bytes| {
        bytes.as_slice() == miliastra_wonderland_music::CONFIG_RELOAD_READY_MARKER
    })
}

#[cfg(target_os = "windows")]
fn latch_replacement_ready(latched: bool, path: &std::path::Path) -> bool {
    latched || replacement_is_ready(path)
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
        let result = miliastra_wonderland_music::run(&config_path);
        return match result {
            Ok(outcome) => Ok(std::process::ExitCode::from(outcome.exit_code())),
            Err(error)
                if std::env::var_os(miliastra_wonderland_music::CONFIG_RELOAD_CHILD_ENV)
                    .is_some()
                    && !miliastra_wonderland_music::config_reload_child_ready() =>
            {
                eprintln!("配置重载替代进程在 ready 前启动失败: {error:#}");
                Ok(std::process::ExitCode::from(
                    miliastra_wonderland_music::CONFIG_RELOAD_STARTUP_FAILURE_EXIT_CODE,
                ))
            }
            Err(error) => Err(error),
        };
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

    let mut next_child_handoff = WatchdogChildHandoff::default();
    let mut cached_restart_ms = miliastra_wonderland_music::watchdog_restart_ms(&config_path)
        .unwrap_or_else(|error| {
            eprintln!(
                "读取看门狗重启间隔失败，暂用默认值 {DEFAULT_WATCHDOG_RESTART_MS}ms: {error:#}"
            );
            DEFAULT_WATCHDOG_RESTART_MS
        });
    loop {
        let handoff = next_child_handoff;
        let ready_file = handoff.config_reload.then(replacement_ready_file);
        let mut command = Command::new(&current_exe);
        configure_watchdog_child(&mut command, &config_path, handoff, ready_file.as_deref());
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                eprintln!(
                    "启动监听子进程失败，将在 {cached_restart_ms}ms 后重试: {}: {error:#}",
                    current_exe.display()
                );
                sleep(Duration::from_millis(cached_restart_ms));
                continue;
            }
        };
        let mut replacement_ready = false;
        let status = if let Some(path) = ready_file.as_deref() {
            loop {
                replacement_ready = latch_replacement_ready(replacement_ready, path);
                if replacement_ready {
                    break child
                        .wait()
                        .context("等待已 ready 的配置重载替代进程退出")?;
                }
                if let Some(status) = child.try_wait().context("轮询配置重载替代进程状态")?
                {
                    break status;
                }
                sleep(Duration::from_millis(100));
            }
        } else {
            child.wait().context("等待监听子进程退出")?
        };
        // Close the final check-vs-exit window: the child can publish readiness immediately
        // before it exits, after the preceding poll but before `try_wait` observes termination.
        if let Some(path) = ready_file.as_deref() {
            replacement_ready = latch_replacement_ready(replacement_ready, path);
        }
        if let Some(path) = ready_file
            && let Err(error) = std::fs::remove_file(&path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("清理配置重载 ready 标记失败: {}: {error}", path.display());
        }
        if status.success() {
            return Ok(std::process::ExitCode::SUCCESS);
        }
        next_child_handoff =
            WatchdogChildHandoff::after_exit(handoff, status.code(), replacement_ready);
        if next_child_handoff.config_reload {
            if is_config_reload_exit(status.code()) {
                eprintln!("监听子进程已完成配置重载关停，立即重新启动");
            } else {
                eprintln!(
                    "配置重载替代进程尚未 ready，继续使用热启动路径；{cached_restart_ms}ms 后重试"
                );
                sleep(Duration::from_millis(cached_restart_ms));
            }
            continue;
        }

        match miliastra_wonderland_music::watchdog_restart_ms(&config_path) {
            Ok(restart_ms) => cached_restart_ms = restart_ms,
            Err(error) => {
                eprintln!("读取最新看门狗重启间隔失败，继续使用 {cached_restart_ms}ms: {error:#}")
            }
        }
        eprintln!("监听子进程异常退出: status={status}，{cached_restart_ms}ms 后重启");
        sleep(Duration::from_millis(cached_restart_ms));
    }
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use std::ffi::OsStr;
    use std::path::Path;
    use std::process::Command;

    use super::{
        WatchdogChildHandoff, configure_watchdog_child, is_config_reload_exit,
        latch_replacement_ready, replacement_is_ready,
    };

    #[test]
    fn watchdog_only_treats_the_reserved_code_as_a_configuration_reload() {
        assert!(is_config_reload_exit(Some(75)));
        assert!(is_config_reload_exit(Some(77)));
        assert!(!is_config_reload_exit(Some(0)));
        assert!(!is_config_reload_exit(Some(1)));
        assert!(!is_config_reload_exit(None));
    }

    #[test]
    fn configuration_reload_exit_marks_the_following_child_only() {
        let reload_child =
            WatchdogChildHandoff::after_exit(WatchdogChildHandoff::default(), Some(75), false);

        assert!(reload_child.config_reload);
        assert!(!WatchdogChildHandoff::after_exit(reload_child, Some(1), true).config_reload);
        assert!(
            !WatchdogChildHandoff::after_exit(WatchdogChildHandoff::default(), None, false)
                .config_reload
        );
    }

    #[test]
    fn any_pre_ready_replacement_failure_preserves_reload_handoff_for_the_next_child() {
        let reload_child = WatchdogChildHandoff {
            config_reload: true,
            run_startup: true,
        };
        assert!(
            WatchdogChildHandoff::after_exit(
                reload_child,
                Some(i32::from(
                    miliastra_wonderland_music::CONFIG_RELOAD_STARTUP_FAILURE_EXIT_CODE,
                )),
                false,
            )
            .config_reload
        );
        assert!(WatchdogChildHandoff::after_exit(reload_child, Some(101), false).config_reload);
        assert!(!WatchdogChildHandoff::after_exit(reload_child, Some(1), true).config_reload);
        assert!(
            WatchdogChildHandoff::after_exit(reload_child, Some(101), false).run_startup,
            "pre-ready retries must preserve the startup handoff"
        );
    }

    #[test]
    fn watchdog_accepts_only_the_complete_ready_marker() {
        let root = std::env::temp_dir().join(format!(
            "miliastra-watchdog-ready-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let marker = root.join("replacement.ready");

        assert!(!replacement_is_ready(&marker));
        std::fs::write(&marker, []).unwrap();
        assert!(!replacement_is_ready(&marker));
        std::fs::write(&marker, b"ready").unwrap();
        assert!(!replacement_is_ready(&marker));
        std::fs::write(
            &marker,
            miliastra_wonderland_music::CONFIG_RELOAD_READY_MARKER,
        )
        .unwrap();
        assert!(replacement_is_ready(&marker));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn observed_readiness_stays_latched_after_marker_removal() {
        let root = std::env::temp_dir().join(format!(
            "miliastra-watchdog-ready-latch-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let marker = root.join("replacement.ready");
        std::fs::write(
            &marker,
            miliastra_wonderland_music::CONFIG_RELOAD_READY_MARKER,
        )
        .unwrap();

        let latched = latch_replacement_ready(false, &marker);
        assert!(latched);
        std::fs::remove_file(&marker).unwrap();
        assert!(latch_replacement_ready(latched, &marker));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn watchdog_child_environment_carries_scoped_reload_marker() {
        let mut reload_command = Command::new("child.exe");
        configure_watchdog_child(
            &mut reload_command,
            Path::new(r"C:\Miliastra\config.yaml"),
            WatchdogChildHandoff {
                config_reload: true,
                run_startup: true,
            },
            Some(Path::new(r"C:\Miliastra\replacement.ready")),
        );
        assert_eq!(
            command_environment(&reload_command, "MILIASTRA_CONFIG_RELOAD_CHILD"),
            Some(OsStr::new("1"))
        );
        assert_eq!(
            command_environment(&reload_command, "MILIASTRA_CONFIG_RELOAD_READY_FILE"),
            Some(OsStr::new(r"C:\Miliastra\replacement.ready"))
        );
        assert_eq!(
            command_environment(&reload_command, "MILIASTRA_CONFIG_RELOAD_RUN_STARTUP"),
            Some(OsStr::new("1"))
        );

        let mut crash_restart_command = Command::new("child.exe");
        configure_watchdog_child(
            &mut crash_restart_command,
            Path::new(r"C:\Miliastra\config.yaml"),
            WatchdogChildHandoff::after_exit(WatchdogChildHandoff::default(), Some(1), false),
            None,
        );
        assert_eq!(
            command_environment(&crash_restart_command, "MILIASTRA_CONFIG_RELOAD_CHILD"),
            None
        );
        assert_eq!(
            command_environment(&crash_restart_command, "MILIASTRA_CONFIG_RELOAD_READY_FILE"),
            None
        );
        assert_eq!(
            command_environment(
                &crash_restart_command,
                "MILIASTRA_CONFIG_RELOAD_RUN_STARTUP"
            ),
            None
        );
    }

    fn command_environment<'a>(command: &'a Command, name: &str) -> Option<&'a OsStr> {
        command
            .get_envs()
            .find_map(|(key, value)| (key == name).then_some(value).flatten())
    }
}
