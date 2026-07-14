use std::io::IsTerminal;
use std::path::Path;

use anyhow::Result;

use crate::app::AutomationApp;
use crate::app::logger;
use crate::app::monitor::MonitorShared;
use crate::app::queue::PersistentQueue;
use crate::app::runtime_state::PersistentRuntimeState;
use crate::app::song_dedup::PersistentSongDedupHistory;
use crate::app::tui::TuiHandle;
use crate::config::AppConfig;

pub(crate) fn run(config_path: &Path) -> Result<()> {
    let config = AppConfig::load_or_create(config_path)?;
    let monitor = MonitorShared::new(config.tui.log_lines);
    let tui_handle = if config.tui.enabled && std::io::stdout().is_terminal() {
        match TuiHandle::start(&config.tui, monitor.clone()) {
            Ok(handle) => Some(handle),
            Err(error) => {
                eprintln!("TUI 启动失败，回退普通日志输出: {error:#}");
                None
            }
        }
    } else if config.tui.enabled {
        eprintln!("检测到非交互终端，已关闭 TUI");
        None
    } else {
        None
    };
    let log_paths = logger::init(
        &config.logging,
        Some(monitor.log_sink()),
        tui_handle.is_none(),
    )?;
    log::info!("日志文件: {}", log_paths.main.display());
    log::info!("性能日志文件: {}", log_paths.timing.display());
    log::info!("配置文件: {}", config_path.display());
    log::info!(
        "HTTP/Web 面板: {}:{} enabled={}",
        config.http.host,
        config.http.port,
        config.http.enabled
    );
    log::info!(
        "FeelUOwn: {}:{}",
        config.feeluown.host,
        config.feeluown.port
    );

    let mut runtime_state = PersistentRuntimeState::load(config.state.runtime_state_path.clone())?;
    if runtime_state.state_mut().clear_hall_countdown_cache() {
        runtime_state.save()?;
        log::info!("启动时已清理上次运行的大厅倒计时缓存，等待本次大厅检测重新确认");
    }
    let queue = PersistentQueue::load(config.state.queue_path.clone(), config.queue.max_size)?;
    let song_dedup_history =
        PersistentSongDedupHistory::load(config.song_dedup.history_path.clone())?;
    log::info!("已加载队列: {} 首", queue.len());
    log::info!("已加载长时间同歌去重历史: {} 条", song_dedup_history.len());
    log::info!(
        "已加载运行时状态: playback_state={:?}",
        runtime_state.state().playback.state
    );

    let mut app = AutomationApp::new(config, runtime_state, queue, song_dedup_history, monitor)?;
    let result = app.run();
    drop(tui_handle);
    result
}
