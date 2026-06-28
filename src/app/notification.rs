use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::json;

pub fn send_windows_notification(title: &str, message: &str) -> bool {
    let script = r#"
$json = [Console]::In.ReadToEnd()
$data = $json | ConvertFrom-Json
Add-Type -AssemblyName System.Windows.Forms
$notify = New-Object System.Windows.Forms.NotifyIcon
$notify.Icon = [System.Drawing.SystemIcons]::Information
$notify.BalloonTipTitle = [string]$data.title
$notify.BalloonTipText = [string]$data.message
$notify.Visible = $true
$notify.ShowBalloonTip(5000)
Start-Sleep -Milliseconds 6000
$notify.Dispose()
"#;
    let payload = json!({
        "title": if title.is_empty() { "点歌命令待处理" } else { title },
        "message": message,
    })
    .to_string();
    match Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(payload.as_bytes());
            }
            true
        }
        Err(error) => {
            log::error!("Windows 通知启动失败: {error}");
            false
        }
    }
}
