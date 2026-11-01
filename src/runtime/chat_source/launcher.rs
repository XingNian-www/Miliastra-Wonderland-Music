//! Miliastra-Wonderland-ChatBridge 进程管理（"登录器"角色）。
//!
//! Miliastra-Wonderland-ChatBridge 是独立可执行文件：它负责拉起游戏、注入 DLL、嗅探聊天包，
//! 并通过 HTTP/WS 暴露消息。本模块负责：
//!   1. 判断它是否已经在监听；
//!   2. 没在监听时替用户把它拉起来；
//!   3. 消费它的退出码，决定「正常重拉 / 等玩家退出 / 指数退避 / 停止自动重启」。
//!
//! 不管理游戏生命周期：ChatBridge 的生命周期跟随游戏（游戏退出它自己也退出），
//! BOT 退出时**不**停止它——否则玩家还开着游戏却被掐断聊天源。
//!
//! 退出码契约（来自 ChatBridge README，发布后不再变）：
//!   0 = 正常结束；1 = 启动错误；3 = 游戏已在运行；4 = 链路丢失且游戏仍活着。
//! 旧实现只 spawn 不 wait，所有失败都被当成「没启动」，于是每 30 秒盲目重拉，
//! 既看不到启动错误，也会在退出码 4 时无限重启一个注定失败的宿主。

use std::fs::OpenOptions;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::config::ChatSourceConfig;

/// 启动失败后的指数退避基数，也是首次重试等待时间。
///
/// 旧实现是固定 30 秒冷却：启动错误（端口占用、DLL 缺失等）会每 30 秒
/// 重试一次，日志里看不出规律。改为按连续失败次数指数增长并封顶。
const LAUNCH_BACKOFF_BASE: Duration = Duration::from_secs(30);

/// 启动失败退避的上限：再慢也至少 10 分钟试一次，避免永久放弃。
const LAUNCH_BACKOFF_MAX: Duration = Duration::from_secs(600);

/// 「等待游戏退出」状态下重复提示的最小间隔。
const WAIT_STATUS_LOG_INTERVAL: Duration = Duration::from_secs(60);

/// 发布包里的可执行文件名（与 Cargo.toml 的 package name 一致）。
const CHATBRIDGE_EXE: &str = "miliastra-wonderland-chatbridge.exe";

// ChatBridge 的稳定退出码契约。不要改含义，只用于本地分类。
const EXIT_OK: i32 = 0;
const EXIT_STARTUP_ERROR: i32 = 1;
const EXIT_GAME_ALREADY_RUNNING: i32 = 3;
const EXIT_SESSION_LOST_GAME_ALIVE: i32 = 4;

/// ChatBridge 退出码的分类结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitKind {
    /// 0：游戏退出、Ctrl-C 或 DLL 主动结束会话。下一轮可正常重拉。
    Normal,
    /// 1：启动错误（端口占用、DLL/metadata 缺失等）。报错并指数退避。
    StartupError,
    /// 3：游戏已在运行（仅 reattach 禁用的兼容路径）。等玩家退出。
    GameAlreadyRunning,
    /// 4：链路丢失且游戏仍在运行。停止自动重启，提示玩家重启游戏。
    SessionLostGameAlive,
    /// 未约定的退出码 / 被信号终止：按启动错误处理并退避。
    Unknown,
}

/// 根据退出码分类。None 表示进程被信号终止（Windows 上少见）。
fn classify_exit(code: Option<i32>) -> ExitKind {
    match code {
        Some(EXIT_OK) => ExitKind::Normal,
        Some(EXIT_STARTUP_ERROR) => ExitKind::StartupError,
        Some(EXIT_GAME_ALREADY_RUNNING) => ExitKind::GameAlreadyRunning,
        Some(EXIT_SESSION_LOST_GAME_ALIVE) => ExitKind::SessionLostGameAlive,
        _ => ExitKind::Unknown,
    }
}

/// 退出码的人类可读描述，写进日志便于和 ChatBridge README 对账。
fn describe_exit_code(code: Option<i32>) -> String {
    match code {
        Some(EXIT_OK) => "0（正常结束）".to_string(),
        Some(EXIT_STARTUP_ERROR) => "1（启动错误）".to_string(),
        Some(EXIT_GAME_ALREADY_RUNNING) => "3（游戏已在运行）".to_string(),
        Some(EXIT_SESSION_LOST_GAME_ALIVE) => "4（链路丢失且游戏仍在运行）".to_string(),
        Some(other) => format!("{other}（未约定）"),
        None => "无（被信号终止）".to_string(),
    }
}

/// 连续失败 n 次后的退避时长：30s、60s、120s、240s、480s，封顶 600s。
fn startup_backoff(failures: u32) -> Duration {
    let exponent = failures.saturating_sub(1).min(5);
    let factor = 1u32 << exponent;
    LAUNCH_BACKOFF_BASE
        .saturating_mul(factor)
        .min(LAUNCH_BACKOFF_MAX)
}

/// 退出码留下的停止态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopMode {
    /// 退出码 3：等游戏进程消失后再重试。
    WaitGameExit,
    /// 退出码 4：停止自动重启，等待玩家重启游戏（本进程内不再自动拉起）。
    LinkLost,
}

/// 启动器状态。所有字段都由后台等待线程与 ensure_running 共享。
struct LaunchState {
    /// 连续启动失败次数，用于指数退避。
    failures: u32,
    /// 下一次允许尝试启动的时刻。
    next_attempt: Option<Instant>,
    /// 退出码留下的停止态。
    stop: Option<StopMode>,
    /// 本次启动的进程是否还在运行（启动中/已拉起，等待退出线程回报）。
    child_running: bool,
    /// 「等待游戏退出 / 退避中」提示的节流时间。
    gate_logged_at: Option<Instant>,
    /// 退出码 4 的错误提示是否已经打印过。
    link_lost_logged: bool,
    /// 退出码 4 停止后是否已经确认过「游戏仍在运行」。
    ///
    /// 只有先看到游戏进程存在、之后又看到它消失，才能判断玩家照提示退出了
    /// 游戏；首次探测就失败（进程枚举瞬时失败等）不能立即解除停止态，
    /// 否则会退化回盲目重拉。
    link_lost_game_seen: bool,
}

impl LaunchState {
    const fn new() -> Self {
        Self {
            failures: 0,
            next_attempt: None,
            stop: None,
            child_running: false,
            gate_logged_at: None,
            link_lost_logged: false,
            link_lost_game_seen: false,
        }
    }
}

static LAUNCH_STATE: Mutex<LaunchState> = Mutex::new(LaunchState::new());

fn launch_state() -> std::sync::MutexGuard<'static, LaunchState> {
    LAUNCH_STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// 启动闸门：决定本次 ensure_running 能不能真的 spawn。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaunchGate {
    /// 可以尝试启动。
    Launch,
    /// 退出码 3 之后，游戏仍在运行，继续等待。
    WaitingForGameExit,
    /// 退出码 4 之后，停止自动重启。
    LinkLost,
    /// 已有一个子进程在运行，不重复启动。
    ChildRunning,
    /// 启动失败后的指数退避尚未到期。
    Backoff,
}

fn log_gate_due(state: &mut LaunchState, now: Instant) -> bool {
    let due = state
        .gate_logged_at
        .is_none_or(|at| now.saturating_duration_since(at) >= WAIT_STATUS_LOG_INTERVAL);
    if due {
        state.gate_logged_at = Some(now);
    }
    due
}

/// 根据现有停止态/退避/子进程状态决定本次是否启动。
///
/// 抽成纯函数是为了能单测状态迁移，不必真的起进程或探测 Windows 进程列表。
///
/// `game_running` 由**调用方在锁外**算好后传入：进程枚举要 CreateToolhelp32Snapshot
/// 并遍历系统全部进程，不能持着 LAUNCH_STATE 做这件事。`None` 表示调用方判断
/// 本次不需要探测（没有停止态要判定），按「看不到游戏」处理。
fn gate_launch(state: &mut LaunchState, now: Instant, game_running: Option<bool>) -> LaunchGate {
    match state.stop {
        Some(StopMode::LinkLost) => {
            let running = game_running.unwrap_or(false);
            if running {
                // 退出码 4 表态时游戏还活着。先记录「确实看到过游戏在跑」，
                // 之后它消失才说明玩家照提示退出了游戏。
                state.link_lost_game_seen = true;
                if log_gate_due(state, now) {
                    log::info!(
                        "ChatBridge 退出码 4 的自动重启停止状态仍在生效；游戏仍在运行，等待玩家退出或重启游戏"
                    );
                }
                return LaunchGate::LinkLost;
            }
            if !state.link_lost_game_seen {
                // 第一次探测就没看到游戏进程，不能据此认定玩家已经退出：
                // 进程枚举可能瞬时失败，也可能是停止态刚置位的瞬间。
                if log_gate_due(state, now) {
                    log::info!(
                        "退出码 4 的停止状态尚未确认过游戏进程，继续等待确认，避免在启动瞬间误判为游戏已退出"
                    );
                }
                return LaunchGate::LinkLost;
            }
            state.stop = None;
            state.link_lost_game_seen = false;
            state.link_lost_logged = false;
            state.gate_logged_at = None;
            log::info!(
                "检测到游戏进程已退出（玩家已按提示退出或重启游戏），解除 ChatBridge 退出码 4 的自动重启停止状态，重新尝试启动"
            );
        }
        Some(StopMode::WaitGameExit) => {
            if game_running.unwrap_or(false) {
                if log_gate_due(state, now) {
                    log::info!(
                        "ChatBridge 上次以退出码 3 结束（游戏已在运行）；游戏仍在运行，继续等待玩家退出后再尝试启动"
                    );
                }
                return LaunchGate::WaitingForGameExit;
            }
            state.stop = None;
            state.gate_logged_at = None;
            log::info!("检测到游戏进程已退出，重新尝试启动 ChatBridge");
        }
        None => {}
    }
    if state.child_running {
        return LaunchGate::ChildRunning;
    }
    if let Some(next) = state.next_attempt
        && now < next
    {
        if log_gate_due(state, now) {
            log::debug!(
                "ChatBridge 启动退避中，约 {} 秒后再试",
                next.saturating_duration_since(now).as_secs()
            );
        }
        return LaunchGate::Backoff;
    }
    state.next_attempt = None;
    LaunchGate::Launch
}

/// 记录一次启动失败并安排指数退避，返回本次退避时长。
fn schedule_startup_backoff(state: &mut LaunchState) -> Duration {
    state.failures = state.failures.saturating_add(1);
    let delay = startup_backoff(state.failures);
    state.next_attempt = Some(Instant::now() + delay);
    delay
}

/// 解析 WS 地址里的 host 与 port。
///
/// 只支持 ws://host:port/path 形式；解析失败时返回 None，调用方据此跳过探测。
fn parse_endpoint(url: &str) -> Option<(String, u16)> {
    let rest = url.strip_prefix("ws://")?;
    let authority = rest.split('/').next()?;
    // 去掉可能存在的 userinfo。
    let authority = authority.rsplit('@').next()?;
    let (host, port) = authority.rsplit_once(':')?;
    Some((host.to_owned(), port.parse().ok()?))
}

/// ChatBridge 是否已经在监听。
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

/// 定位 ChatBridge 可执行文件。
fn locate_executable(config: &ChatSourceConfig) -> Option<PathBuf> {
    let explicit = config.executable.trim();
    if !explicit.is_empty() {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let parent = exe_dir.parent()?.to_path_buf();
    // 常见布局：发布包统一放 deps/bin（与 login_helper 一致），
    // 其次是 BOT 目录旁的解压目录，或与 BOT 同级的同名目录。
    // 发布包目录名带版本号（Miliastra-Wonderland-ChatBridge-v0.1.0-win-x64），
    // 解压后目录名无法预知，所以额外扫描父目录下已解压的发布包目录。
    let mut candidates = vec![
        exe_dir.join("deps").join("bin").join(CHATBRIDGE_EXE),
        exe_dir
            .join("Miliastra-Wonderland-ChatBridge")
            .join(CHATBRIDGE_EXE),
        exe_dir.join(CHATBRIDGE_EXE),
        parent.join("deps").join("bin").join(CHATBRIDGE_EXE),
        parent
            .join("Miliastra-Wonderland-ChatBridge")
            .join(CHATBRIDGE_EXE),
    ];
    candidates.extend(published_candidates(&parent));
    candidates.into_iter().find(|path| path.is_file())
}

/// 扫描目录下一层，找出解压出来的发布包目录里的可执行文件。
///
/// 发布包目录名带版本号（Miliastra-Wonderland-ChatBridge-v0.1.0-win-x64），
/// 用户通常是整个解压到 BOT 旁边，写死目录名会漏掉。
fn published_candidates(parent: &std::path::Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("Miliastra-Wonderland-ChatBridge")
        })
        .map(|entry| entry.path().join(CHATBRIDGE_EXE))
        .filter(|path| path.is_file())
        .collect();
    // 目录顺序来自文件系统，不稳定；排序让"多个版本都存在"时的选择可复现。
    found.sort();
    found
}

/// ChatBridge 的 stdout/stderr 日志路径：放在可执行文件旁边，与它自己的
/// chatbridge.log 分开，避免两个进程抢同一个文件。
fn launch_log_path(exe: &Path) -> PathBuf {
    exe.parent()
        .map(|dir| dir.join("chatbridge-stdout.log"))
        .unwrap_or_else(|| std::env::temp_dir().join("miliastra-chatbridge-stdout.log"))
}

/// 把子进程的 stdout/stderr 重定向到日志文件。
///
/// 旧实现用 Stdio::null()：ChatBridge 启动失败时的错误只剩一个退出码，
/// 看不到端口占用、DLL 缺失等具体原因。日志文件打开失败时退回 null
/// 并告警，保证启动本身不因日志目录不可写而失败。
fn open_launch_stdio(exe: &Path) -> (Stdio, Stdio) {
    let path = launch_log_path(exe);
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(file) => match file.try_clone() {
            Ok(clone) => (Stdio::from(file), Stdio::from(clone)),
            Err(error) => {
                log::warn!(
                    "打开 ChatBridge 启动日志的副本失败（{}），本次 stdout/stderr 不落盘：{error}",
                    path.display()
                );
                (Stdio::null(), Stdio::null())
            }
        },
        Err(error) => {
            log::warn!(
                "无法打开 ChatBridge 启动日志（{}），本次 stdout/stderr 不落盘：{error}",
                path.display()
            );
            (Stdio::null(), Stdio::null())
        }
    }
}

/// 确保 ChatBridge 在运行；必要时拉起。返回 true 表示本次调用启动了新进程。
///
/// target_process 是配置里的游戏进程名列表（window.target_process），
/// 只在消费退出码 3 时用来判断「玩家是否已经退出游戏」。
pub(crate) fn ensure_running(config: &ChatSourceConfig, target_process: &str) -> Result<bool> {
    if !config.auto_start {
        return Ok(false);
    }
    if is_listening(&config.url) {
        // 已在监听：可能由用户手动启动。清掉「等待游戏退出」状态，避免
        // 用户手动拉起 ChatBridge 后还被旧的等待态拦住。
        //
        // 注意不要把 child_running 清零：正在监听的可能就是我们自己拉起的
        // 子进程，清零后它的退出等待线程会被误判成「没有在跑」，有重复启动风险。
        let mut state = launch_state();
        if state.stop == Some(StopMode::WaitGameExit) {
            state.stop = None;
            state.gate_logged_at = None;
        }
        return Ok(false);
    }

    // 先过退出码留下的停止态/退避闸门，避免在等待玩家退出时反复探测与报错。
    let now = Instant::now();
    // 进程枚举必须在锁外做：它要遍历系统全部进程，持锁期间会挡住退出等待线程。
    let needs_process_probe = {
        let state = launch_state();
        matches!(
            state.stop,
            Some(StopMode::LinkLost | StopMode::WaitGameExit)
        )
    };
    let game_running = needs_process_probe.then(|| game_process_running(target_process));
    {
        let mut state = launch_state();
        let gate = gate_launch(&mut state, now, game_running);
        match gate {
            LaunchGate::LinkLost => {
                if !state.link_lost_logged {
                    state.link_lost_logged = true;
                    log::error!(
                        "ChatBridge 上次以退出码 4 结束：聊天链路已丢失且游戏仍在运行，自动重启已停止；请重启游戏后再试"
                    );
                }
                return Ok(false);
            }
            LaunchGate::WaitingForGameExit | LaunchGate::ChildRunning | LaunchGate::Backoff => {
                return Ok(false);
            }
            LaunchGate::Launch => {
                state.child_running = true;
            }
        }
    }

    let exe = match locate_executable(config) {
        Some(exe) => exe,
        None => {
            let mut state = launch_state();
            state.child_running = false;
            let delay = schedule_startup_backoff(&mut state);
            return Err(anyhow::anyhow!(
                "找不到 {CHATBRIDGE_EXE}：请在配置里填写 chat_source.executable，或把它放在 {}、{} 或 {} 下；{delay:?} 后重试",
                "<BOT目录>/deps/bin/",
                "<BOT目录>/Miliastra-Wonderland-ChatBridge/",
                "<BOT目录>/"
            ));
        }
    };
    let Some((_, port)) = parse_endpoint(&config.url) else {
        let mut state = launch_state();
        state.child_running = false;
        let delay = schedule_startup_backoff(&mut state);
        return Err(anyhow::anyhow!(
            "无法从 chat_source.url 解析端口: {}；{delay:?} 后重试",
            config.url
        ));
    };
    let mut command = Command::new(&exe);
    command
        .arg("--port")
        .arg(port.to_string())
        .arg("--token")
        .arg(&config.token)
        .arg("--self-uid")
        .arg(config.self_uid.to_string())
        .stdin(Stdio::null());
    let (stdout, stderr) = open_launch_stdio(&exe);
    command.stdout(stdout).stderr(stderr);
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
    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let mut state = launch_state();
            state.child_running = false;
            let delay = schedule_startup_backoff(&mut state);
            return Err(error).with_context(|| {
                format!(
                    "启动 Miliastra-Wonderland-ChatBridge 失败: {}（{delay:?} 后重试）",
                    exe.display()
                )
            });
        }
    };
    let pid = child.id();
    log::info!(
        "已启动外部聊天桥接程序「{}」，监听端口 {port}，进程号 {pid}；stdout/stderr 写入 {}",
        exe.display(),
        launch_log_path(&exe).display()
    );
    let watched_target = target_process.to_string();
    std::thread::Builder::new()
        .name("chatbridge-exit".into())
        .spawn(move || watch_chatbridge_exit(child, pid, watched_target))
        .map_err(|error| {
            let mut state = launch_state();
            state.child_running = false;
            anyhow::anyhow!("启动 ChatBridge 退出码等待线程失败（pid {pid}）：{error}")
        })?;
    Ok(true)
}

/// 在后台线程里等待子进程退出，消费退出码后更新启动器状态。
fn watch_chatbridge_exit(mut child: Child, pid: u32, target_process: String) {
    match child.wait() {
        Ok(status) => handle_chatbridge_exit(status.code(), pid, &target_process),
        Err(error) => {
            log::error!("等待 ChatBridge 进程退出失败（pid {pid}）：{error}");
            // 拿不到状态时按启动错误退避，避免每轮重连都立刻重拉。
            handle_chatbridge_exit(None, pid, &target_process);
        }
    }
}

/// 按退出码更新自动重启策略。
fn handle_chatbridge_exit(code: Option<i32>, pid: u32, target_process: &str) {
    let kind = classify_exit(code);
    let mut state = launch_state();
    state.child_running = false;
    match kind {
        ExitKind::Normal => {
            state.failures = 0;
            state.next_attempt = None;
            state.stop = None;
            state.link_lost_game_seen = false;
            state.link_lost_logged = false;
            log::info!(
                "ChatBridge 正常结束（{}，pid {pid}）：游戏退出或会话主动结束；下一轮可正常重新拉起",
                describe_exit_code(code)
            );
        }
        ExitKind::StartupError | ExitKind::Unknown => {
            let delay = schedule_startup_backoff(&mut state);
            log::error!(
                "ChatBridge 启动失败（{}，pid {pid}）；已记录错误并将在 {} 秒后重试。请检查启动日志 chatbridge-stdout.log",
                describe_exit_code(code),
                delay.as_secs()
            );
        }
        ExitKind::GameAlreadyRunning => {
            state.failures = 0;
            state.next_attempt = None;
            state.stop = Some(StopMode::WaitGameExit);
            state.link_lost_game_seen = false;
            state.gate_logged_at = Some(Instant::now());
            log::warn!(
                "ChatBridge 以退出码 3 结束（游戏已在运行，目标进程「{target_process}」，pid {pid}）；停止自动重启，等玩家退出游戏后再重试"
            );
        }
        ExitKind::SessionLostGameAlive => {
            state.next_attempt = None;
            state.stop = Some(StopMode::LinkLost);
            state.link_lost_game_seen = false;
            if !state.link_lost_logged {
                state.link_lost_logged = true;
                log::error!(
                    "ChatBridge 以退出码 4 结束（链路丢失且游戏仍在运行，pid {pid}）：已停止自动重启；请退出或重启游戏（检测到游戏进程退出后会自动解除停止并重试）；若游戏内旧 DLL 仍在重连，也可手动重新启动 ChatBridge"
                );
            }
        }
    }
}

/// 按全仓库约定切分目标进程名：逗号、分号、竖线或空白。
///
/// 必须与 window.rs 的 normalize_process_names、game_path.rs 的候选生成使用
/// 同一套分隔符。只按逗号/分号/竖线切分时，"yuanshen.exe GenshinImpact.exe"
/// 会被拼成单个永远匹配不到的 token，退出码 3 的等待逻辑就会立刻误判
/// 「游戏已退出」并退化回盲目重拉。
fn normalize_process_names(value: &str) -> Vec<String> {
    value
        .split(|ch: char| ch == ',' || ch == ';' || ch == '|' || ch.is_whitespace())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| {
            let mut normalized = name.to_ascii_lowercase();
            if !normalized.ends_with(".exe") {
                normalized.push_str(".exe");
            }
            normalized
        })
        .collect()
}

/// 游戏进程是否仍在运行（仅 Windows 有实现；其它平台产品不支持，返回 false）。
#[cfg(windows)]
fn game_process_running(target_process: &str) -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    let targets = normalize_process_names(target_process);
    if targets.is_empty() {
        return false;
    }
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return false;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut has_entry = Process32FirstW(snapshot, &mut entry).is_ok();
        while has_entry {
            let len = entry
                .szExeFile
                .iter()
                .position(|ch| *ch == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..len]).to_ascii_lowercase();
            if targets.iter().any(|target| target == &name) {
                let _ = CloseHandle(snapshot);
                return true;
            }
            has_entry = Process32NextW(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
    }
    false
}

#[cfg(not(windows))]
fn game_process_running(_target_process: &str) -> bool {
    false
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
    fn explicit_executable_wins_over_search_paths() {
        let dir = std::env::temp_dir().join(format!(
            "miliastra-chatbridge-locate-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let exe = dir.join(CHATBRIDGE_EXE);
        std::fs::write(&exe, b"stub").expect("写占位 exe");

        let mut config = ChatSourceConfig {
            executable: exe.display().to_string(),
            ..ChatSourceConfig::default()
        };
        assert_eq!(locate_executable(&config), Some(exe.clone()));

        // 路径不存在时必须回落到搜索路径，而不是直接返回这个坏路径。
        config.executable = dir.join("missing.exe").display().to_string();
        assert_ne!(locate_executable(&config), Some(dir.join("missing.exe")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn published_candidates_accepts_versioned_release_dir() {
        let root = std::env::temp_dir().join(format!(
            "miliastra-chatbridge-published-{}",
            std::process::id()
        ));
        let release = root.join("Miliastra-Wonderland-ChatBridge-v0.1.0-win-x64");
        std::fs::create_dir_all(&release).expect("建发布目录");
        let exe = release.join(CHATBRIDGE_EXE);
        std::fs::write(&exe, b"stub").expect("写占位 exe");

        let found = published_candidates(&root);

        assert_eq!(found, vec![exe]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn auto_start_disabled_never_launches() {
        let config = ChatSourceConfig::default();
        assert!(!config.auto_start);
        assert!(!ensure_running(&config, "yuanshen.exe").expect("未启用时不应报错"));
    }

    /// H3：0/1/3/4 必须被区分；未知码不能当成正常结束。
    #[test]
    fn exit_codes_classify_into_distinct_actions() {
        assert_eq!(classify_exit(Some(0)), ExitKind::Normal);
        assert_eq!(classify_exit(Some(1)), ExitKind::StartupError);
        assert_eq!(classify_exit(Some(3)), ExitKind::GameAlreadyRunning);
        assert_eq!(classify_exit(Some(4)), ExitKind::SessionLostGameAlive);
        assert_eq!(classify_exit(Some(2)), ExitKind::Unknown);
        assert_eq!(classify_exit(Some(75)), ExitKind::Unknown);
        assert_eq!(classify_exit(None), ExitKind::Unknown);
    }

    /// H3：启动错误按连续失败次数指数退避，并在 600 秒封顶。
    #[test]
    fn startup_backoff_grows_exponentially_then_caps() {
        assert_eq!(startup_backoff(1), Duration::from_secs(30));
        assert_eq!(startup_backoff(2), Duration::from_secs(60));
        assert_eq!(startup_backoff(3), Duration::from_secs(120));
        assert_eq!(startup_backoff(4), Duration::from_secs(240));
        assert_eq!(startup_backoff(5), Duration::from_secs(480));
        assert_eq!(startup_backoff(6), Duration::from_secs(600));
        assert_eq!(startup_backoff(100), Duration::from_secs(600));
    }

    /// H3：退出码 3 之后必须等游戏退出；游戏还在时绝不重拉。
    #[test]
    fn exit_code_3_waits_for_game_process_to_disappear() {
        let mut state = LaunchState::new();
        state.stop = Some(StopMode::WaitGameExit);
        let now = Instant::now();
        assert_eq!(
            gate_launch(&mut state, now, Some(true)),
            LaunchGate::WaitingForGameExit
        );
        assert_eq!(state.stop, Some(StopMode::WaitGameExit));
        // 游戏退出后解除等待，下一次调用才允许启动。
        assert_eq!(gate_launch(&mut state, now, Some(false)), LaunchGate::Launch);
        assert_eq!(state.stop, None);
    }

    /// H3：退出码 4 之后停止自动重启；在确认过游戏进程之前不会被瞬时误判解除。
    #[test]
    fn exit_code_4_stops_automatic_relaunch() {
        let mut state = LaunchState::new();
        state.stop = Some(StopMode::LinkLost);
        let now = Instant::now();
        for _ in 0..5 {
            assert_eq!(
                gate_launch(&mut state, now, Some(false)),
                LaunchGate::LinkLost,
                "退出码 4 是人工介入信号，不能自动重拉"
            );
        }
        assert!(!state.link_lost_game_seen);
    }

    /// 复审 Medium：退出码 4 的停止态可被「确认过游戏在跑、随后游戏消失」解除。
    ///
    /// 玩家按日志提示退出/重启游戏后，BOT 应重新尝试拉起 ChatBridge，
    /// 而不是只能重启 BOT 进程。
    #[test]
    fn exit_code_4_unlocks_after_game_process_disappears() {
        let mut state = LaunchState::new();
        state.stop = Some(StopMode::LinkLost);
        let now = Instant::now();
        // 先确认游戏仍在运行：停止态继续，并记住确实看到过游戏。
        assert_eq!(gate_launch(&mut state, now, Some(true)), LaunchGate::LinkLost);
        assert!(state.link_lost_game_seen);
        // 玩家照提示退出游戏：游戏进程消失后解除停止态并允许重试。
        assert_eq!(gate_launch(&mut state, now, Some(false)), LaunchGate::Launch);
        assert_eq!(state.stop, None);
        assert!(!state.link_lost_game_seen);
    }

    /// 复审 Medium：首次探测就看不到游戏时不能解除——区分「从未检测到」与
    /// 「检测到后又消失」，避免停止态刚置位的瞬间误判成玩家已退出。
    #[test]
    fn exit_code_4_does_not_unlock_on_first_missed_probe() {
        let mut state = LaunchState::new();
        state.stop = Some(StopMode::LinkLost);
        assert_eq!(
            gate_launch(&mut state, Instant::now(), Some(false)),
            LaunchGate::LinkLost
        );
        assert_eq!(state.stop, Some(StopMode::LinkLost));
        assert!(!state.link_lost_game_seen);
    }

    /// H3：启动失败退避未到期时不启动，到期后放行。
    #[test]
    fn backoff_blocks_until_due() {
        let mut state = LaunchState::new();
        let now = Instant::now();
        state.next_attempt = Some(now + Duration::from_secs(30));
        assert_eq!(gate_launch(&mut state, now, Some(false)), LaunchGate::Backoff);
        assert_eq!(
            gate_launch(&mut state, now + Duration::from_secs(31), Some(false)),
            LaunchGate::Launch
        );
    }

    /// H3：已有一个子在运行时不重复启动。
    #[test]
    fn child_running_prevents_duplicate_launch() {
        let mut state = LaunchState::new();
        state.child_running = true;
        assert_eq!(
            gate_launch(&mut state, Instant::now(), Some(false)),
            LaunchGate::ChildRunning
        );
        state.child_running = false;
        assert_eq!(
            gate_launch(&mut state, Instant::now(), Some(false)),
            LaunchGate::Launch
        );
    }

    /// 复审 Low-1：target_process 必须支持空白分隔，与 window.rs / game_path.rs 一致。
    ///
    /// 旧代码只按逗号/分号/竖线切分，空格分隔的合法配置会被拼成一个
    /// 永远匹配不到的进程名，退出码 3 的等待逻辑随即误判游戏已退出。
    #[test]
    fn target_process_names_accept_whitespace_separators() {
        assert_eq!(
            normalize_process_names("yuanshen.exe GenshinImpact.exe"),
            vec!["yuanshen.exe".to_string(), "genshinimpact.exe".to_string()]
        );
        assert_eq!(
            normalize_process_names("A.exe;B.exe|C.exe  D.exe"),
            vec![
                "a.exe".to_string(),
                "b.exe".to_string(),
                "c.exe".to_string(),
                "d.exe".to_string(),
            ]
        );
        assert!(normalize_process_names("  , ; |  ").is_empty());
    }

    #[test]
    fn launch_log_path_sits_next_to_executable() {
        let exe = std::env::temp_dir().join("deps/bin/miliastra-wonderland-chatbridge.exe");
        assert_eq!(
            launch_log_path(&exe),
            std::env::temp_dir().join("deps/bin/chatbridge-stdout.log")
        );
    }
}
