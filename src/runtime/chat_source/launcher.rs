//! Miliastra-Wonderland-Chat 进程管理（"登录器"角色）。
//!
//! Miliastra-Wonderland-Chat 是独立可执行文件：它负责拉起游戏、注入 DLL、嗅探聊天包，
//! 并通过 HTTP/WS 暴露消息。本模块只做两件事：
//!   1. 判断它是否已经在监听；
//!   2. 没在监听时替用户把它拉起来。
//!
//! 不管理生命周期：Miliastra-Wonderland-Chat 的生命周期跟随游戏（游戏退出它自己也退出），
//! BOT 退出时**不**停止它——否则玩家还开着游戏却被掐断聊天源。

use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::config::ChatSourceConfig;

/// 启动失败后的冷却时间。
///
/// Miliastra-Wonderland-Chat 会因"游戏已在运行"主动退出；没有冷却的话 BOT 会每 3 秒重启一个
/// 注定失败的进程。这段时间也让用户有机会去关掉游戏。
const LAUNCH_COOLDOWN: Duration = Duration::from_secs(30);

/// 上次尝试启动的时刻。只用于退避，不记录 PID（不管理生命周期）。
static LAST_ATTEMPT: Mutex<Option<Instant>> = Mutex::new(None);

/// 解析 WS 地址里的 host 与 port。
///
/// 只支持 `ws://host:port/path` 形式；解析失败时返回 None，调用方据此跳过探测。
fn parse_endpoint(url: &str) -> Option<(String, u16)> {
    let rest = url.strip_prefix("ws://")?;
    let authority = rest.split('/').next()?;
    // 去掉可能存在的 userinfo。
    let authority = authority.rsplit('@').next()?;
    let (host, port) = authority.rsplit_once(':')?;
    Some((host.to_owned(), port.parse().ok()?))
}

/// Miliastra-Wonderland-Chat 是否已经在监听。
///
/// 用 TCP 连接探测而不是找进程：用户可能把它装在别的目录、用了别的名字。
pub(crate) fn is_listening(url: &str) -> bool {
    let Some((host, port)) = parse_endpoint(url) else {
        return false;
    };
    let Ok(mut addrs) = (host.as_str(), port).to_socket_addrs() else {
        return false;
    };
    addrs.any(|addr| TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok())
}

/// 定位 Miliastra-Wonderland-Chat 可执行文件。
fn locate_executable(config: &ChatSourceConfig) -> Option<PathBuf> {
    let explicit = config.executable.trim();
    if !explicit.is_empty() {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    // 常见布局：BOT 目录旁的 Miliastra-Wonderland-Chat/，或与 BOT 同级的同名目录。
    let candidates = [
        exe_dir.join("Miliastra-Wonderland-Chat").join("miliastra-wonderland-chat.exe"),
        exe_dir.join("miliastra-wonderland-chat.exe"),
        exe_dir.parent()?.join("Miliastra-Wonderland-Chat").join("miliastra-wonderland-chat.exe"),
    ];
    candidates.into_iter().find(|path| path.is_file())
}

/// 确保 Miliastra-Wonderland-Chat 在运行；必要时拉起。
///
/// 返回 true 表示本次调用启动了新进程。
pub(crate) fn ensure_running(config: &ChatSourceConfig) -> Result<bool> {
    if !config.auto_start {
        return Ok(false);
    }
    if is_listening(&config.url) {
        return Ok(false);
    }
    // 冷却期内不重复尝试：上一次可能因游戏已在运行而立刻退出。
    {
        let mut last = LAST_ATTEMPT.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(at) = *last
            && at.elapsed() < LAUNCH_COOLDOWN
        {
            return Ok(false);
        }
        *last = Some(Instant::now());
    }
    let Some((_, port)) = parse_endpoint(&config.url) else {
        anyhow::bail!("无法从 chat_source.url 解析端口: {}", config.url);
    };
    let exe = locate_executable(config).with_context(|| {
        format!(
            "找不到 miliastra-wonderland-chat.exe：请在配置里填写 chat_source.executable，或把它放在 {} 或 {} 下",
            "<BOT目录>/Miliastra-Wonderland-Chat/", "<BOT目录>/"
        )
    })?;
    let mut command = Command::new(&exe);
    command
        .arg("--port")
        .arg(port.to_string())
        .arg("--token")
        .arg(&config.token)
        .arg("--self-uid")
        .arg(config.self_uid.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(dir) = exe.parent() {
        command.current_dir(dir);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW：不要在玩家桌面上闪一个控制台窗口。
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
        .spawn()
        .with_context(|| format!("启动 Miliastra-Wonderland-Chat 失败: {}", exe.display()))?;
    log::info!("已拉起 Miliastra-Wonderland-Chat: {} (端口 {port})", exe.display());
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_documented_stream_url() {
        // 文档给的默认地址。
        assert_eq!(
            parse_endpoint("ws://127.0.0.1:18889/stream"),
            Some(("127.0.0.1".to_string(), 18889))
        );
    }

    #[test]
    fn parses_url_without_path() {
        assert_eq!(
            parse_endpoint("ws://127.0.0.1:18889"),
            Some(("127.0.0.1".to_string(), 18889))
        );
    }

    #[test]
    fn rejects_non_ws_and_missing_port() {
        assert_eq!(parse_endpoint("http://127.0.0.1:18889/stream"), None);
        assert_eq!(parse_endpoint("ws://127.0.0.1/stream"), None);
        assert_eq!(parse_endpoint(""), None);
    }

    #[test]
    fn detects_listening_port() {
        // 绑一个随机端口，确认探测能发现它。
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定");
        let port = listener.local_addr().expect("地址").port();
        let url = format!("ws://127.0.0.1:{port}/stream");
        assert!(is_listening(&url), "监听中的端口应被探测到");
    }

    #[test]
    fn reports_closed_port_as_not_listening() {
        // 绑完立刻释放，端口几乎不可能正好被占用。
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定");
        let port = listener.local_addr().expect("地址").port();
        drop(listener);
        let url = format!("ws://127.0.0.1:{port}/stream");
        assert!(!is_listening(&url));
    }

    #[test]
    fn auto_start_disabled_never_launches() {
        let config = ChatSourceConfig::default();
        assert!(!config.auto_start);
        assert!(!ensure_running(&config).expect("未启用时不应报错"));
    }
}
