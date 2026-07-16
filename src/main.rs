#[cfg(not(target_os = "windows"))]
fn main() {
    compile_error!("miliastra-wonderland-music only supports Windows.");
}

#[cfg(target_os = "windows")]
fn main() -> anyhow::Result<()> {
    use std::path::Path;
    use std::process::Command;
    use std::thread::sleep;
    use std::time::Duration;

    use anyhow::Context;

    let config_path = Path::new("config.yaml");
    if std::env::var_os("MILIASTRA_WATCHDOG_CHILD").is_some() {
        return miliastra_wonderland_music::run(config_path);
    }

    loop {
        let current_exe = std::env::current_exe().context("locate current executable")?;
        let mut child = Command::new(&current_exe)
            .env("MILIASTRA_WATCHDOG_CHILD", "1")
            .spawn()
            .with_context(|| format!("启动监听子进程失败: {}", current_exe.display()))?;
        let status = child.wait().context("等待监听子进程退出")?;
        if status.success() {
            return Ok(());
        }

        let restart_ms = miliastra_wonderland_music::watchdog_restart_ms(config_path)?;
        eprintln!("监听子进程异常退出: status={status}，{restart_ms}ms 后重启");
        sleep(Duration::from_millis(restart_ms));
    }
}
