#[cfg(not(target_os = "windows"))]
fn main() {
    compile_error!("miliastra-wonderland-music only supports Windows.");
}

#[cfg(target_os = "windows")]
mod app {
    mod ai;
    mod chat_output;
    mod clipboard;
    mod command;
    mod config;
    mod config_migration;
    mod dpi;
    mod feeluown;
    mod hotkeys;
    mod http_server;
    mod logger;
    mod manual_tools;
    mod ocr;
    mod queue;
    mod runtime_state;
    mod song_matcher;
    mod window;

    use std::cmp::Ordering;
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::Command as ProcessCommand;
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
    use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
    use std::thread::{self, sleep};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use self::chat_output::ChatOutput;
    use self::command::{CommandLockState, ParsedCommand, PendingCommand, UserCommand};
    use self::config::{AppConfig, PointConfig, RectConfig};
    use self::feeluown::{FeelUOwnClient, PlayerStatus, format_lyrics, format_status};
    use self::ocr::{OcrArgs, make_ocr_engine, merged_ocr_text, recognize_lines};
    use self::queue::PersistentQueue;
    use self::runtime_state::{HALL_EXPIRING_WARNING_MINUTES, PersistentRuntimeState};
    use anyhow::{Context, Result, anyhow, bail};
    use clap::{Args, Parser, Subcommand};
    use enigo::{Direction, Enigo, Key, Keyboard, Settings};
    use image::imageops::FilterType;
    use image::{DynamicImage, GenericImageView, GrayImage, RgbImage};
    use log::{LevelFilter, Log, Metadata, Record, SetLoggerError};
    use ocr_rs::OcrEngine;
    use serde::Serialize;
    use template_matching::{
        Image as MatchImage, MatchTemplateMethod, find_extremes, match_template,
    };

    const CHAT_MARKER_SEARCH_WIDTH: u32 = 60;
    const HALL_INFO_OCR_SAMPLES: usize = 3;
    const IDLE_EXIT_MIN_MINUTES: u32 = 15;
    const OCR_REBUILD_INTERVAL: Duration = Duration::from_secs(60 * 60);
    const OCR_REBUILD_RETRY_INTERVAL: Duration = Duration::from_secs(5 * 60);

    static WINDOW_CAPTURE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    #[derive(Parser, Debug)]
    #[command(
        version,
        about = "Pure Rust OCR/template/input prototype for song-request"
    )]
    struct Cli {
        #[arg(long, default_value = "config.yaml", global = true)]
        config: PathBuf,
        #[arg(long, hide = true, global = true)]
        watchdog_child: bool,
        #[command(flatten)]
        canvas: CanvasArgs,
        #[command(subcommand)]
        command: Option<Command>,
    }

    #[derive(Args, Clone, Debug)]
    struct CanvasArgs {
        #[arg(long, default_value_t = 1920)]
        canvas_width: u32,
        #[arg(long, default_value_t = 1080)]
        canvas_height: u32,
        #[arg(long)]
        no_resize_canvas: bool,
    }

    #[derive(Args, Clone, Debug)]
    struct FrameArgs {
        #[arg(long)]
        image: Option<PathBuf>,
    }

    #[derive(Args, Clone, Debug, Default)]
    struct TemplateArgs {
        #[arg(long)]
        blue_template: Option<PathBuf>,
        #[arg(long)]
        yellow_template: Option<PathBuf>,
        #[arg(long)]
        pink_template: Option<PathBuf>,
        #[arg(long)]
        marker_threshold: Option<f32>,
    }

    #[derive(Clone, Debug)]
    pub(super) struct ResolvedTemplateArgs {
        blue_template: PathBuf,
        yellow_template: PathBuf,
        pink_template: PathBuf,
        marker_threshold: f32,
        marker_dedupe_x: i32,
        marker_dedupe_y: i32,
        text_left_gap: i32,
        block_top_padding: i32,
        block_bottom_padding: i32,
        max_block_height: i32,
        next_marker_min_gap: i32,
        right_padding: i32,
        same_line_y_tolerance: i32,
    }

    impl TemplateArgs {
        fn resolve(&self, config: &AppConfig) -> ResolvedTemplateArgs {
            ResolvedTemplateArgs {
                blue_template: self
                    .blue_template
                    .clone()
                    .unwrap_or_else(|| config.templates.blue_marker.clone()),
                yellow_template: self
                    .yellow_template
                    .clone()
                    .unwrap_or_else(|| config.templates.yellow_marker.clone()),
                pink_template: self
                    .pink_template
                    .clone()
                    .unwrap_or_else(|| config.templates.pink_marker.clone()),
                marker_threshold: self
                    .marker_threshold
                    .unwrap_or(config.templates.marker_threshold),
                marker_dedupe_x: config.ocr.marker_dedupe_x,
                marker_dedupe_y: config.ocr.marker_dedupe_y,
                text_left_gap: config.ocr.text_left_gap,
                block_top_padding: config.ocr.block_top_padding,
                block_bottom_padding: config.ocr.block_bottom_padding,
                max_block_height: config.ocr.max_block_height,
                next_marker_min_gap: config.ocr.next_marker_min_gap,
                right_padding: config.ocr.right_padding,
                same_line_y_tolerance: config.ocr.same_line_y_tolerance,
            }
        }
    }

    #[derive(Args, Clone, Debug, Default)]
    struct UiTemplateArgs {
        #[arg(long)]
        enter_template: Option<PathBuf>,
        #[arg(long)]
        dating_template: Option<PathBuf>,
        #[command(flatten)]
        chat_templates: TemplateArgs,
    }

    #[derive(Clone, Debug)]
    struct ResolvedUiTemplateArgs {
        enter_template: PathBuf,
        dating_template: PathBuf,
        chat_templates: ResolvedTemplateArgs,
    }

    impl UiTemplateArgs {
        fn resolve(&self, config: &AppConfig) -> ResolvedUiTemplateArgs {
            ResolvedUiTemplateArgs {
                enter_template: self
                    .enter_template
                    .clone()
                    .unwrap_or_else(|| config.templates.enter.clone()),
                dating_template: self
                    .dating_template
                    .clone()
                    .unwrap_or_else(|| config.templates.dating.clone()),
                chat_templates: self.chat_templates.resolve(config),
            }
        }
    }

    #[derive(Subcommand, Debug)]
    enum Command {
        Run,
        Manual,
        OcrImage {
            #[command(flatten)]
            ocr: OcrArgs,
            #[arg(long)]
            image: PathBuf,
        },
        OcrRegion {
            #[command(flatten)]
            frame: FrameArgs,
            #[command(flatten)]
            ocr: OcrArgs,
            #[arg(long)]
            rect: String,
        },
        ScanChat {
            #[command(flatten)]
            frame: FrameArgs,
            #[command(flatten)]
            ocr: OcrArgs,
            #[command(flatten)]
            templates: TemplateArgs,
        },
        UiState {
            #[command(flatten)]
            frame: FrameArgs,
            #[command(flatten)]
            templates: UiTemplateArgs,
        },
        HallName {
            #[command(flatten)]
            frame: FrameArgs,
            #[command(flatten)]
            ocr: OcrArgs,
        },
        MatchTemplate {
            #[command(flatten)]
            frame: FrameArgs,
            #[arg(long)]
            template: PathBuf,
            #[arg(long)]
            rect: Option<String>,
            #[arg(long)]
            threshold: Option<f32>,
        },
        ClickTemplate {
            #[command(flatten)]
            frame: FrameArgs,
            #[arg(long)]
            template: PathBuf,
            #[arg(long)]
            rect: String,
            #[arg(long)]
            threshold: Option<f32>,
            #[arg(long)]
            execute: bool,
        },
        Click {
            #[arg(long)]
            x: i32,
            #[arg(long)]
            y: i32,
            #[arg(long)]
            execute: bool,
        },
        Key {
            #[arg(long)]
            key: String,
            #[arg(long)]
            execute: bool,
        },
        SendChat {
            #[arg(long)]
            message: String,
            #[arg(long)]
            execute: bool,
        },
    }

    #[derive(Clone, Copy, Debug, Serialize)]
    struct Point {
        x: i32,
        y: i32,
    }

    impl Point {
        const fn new(x: i32, y: i32) -> Self {
            Self { x, y }
        }
    }

    #[derive(Clone, Copy, Debug, Serialize)]
    struct Rect {
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    }

    impl Rect {
        const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
            Self {
                x,
                y,
                width,
                height,
            }
        }

        fn right(self) -> i32 {
            self.x + self.width as i32
        }

        fn bottom(self) -> i32 {
            self.y + self.height as i32
        }

        fn center(self) -> Point {
            Point::new(
                self.x + self.width as i32 / 2,
                self.y + self.height as i32 / 2,
            )
        }
    }

    impl From<RectConfig> for Rect {
        fn from(value: RectConfig) -> Self {
            Self::new(value.x, value.y, value.width, value.height)
        }
    }

    #[derive(Clone, Debug)]
    struct Canvas {
        width: u32,
        height: u32,
        resize: bool,
    }

    #[derive(Debug)]
    struct Frame {
        image: DynamicImage,
    }

    #[derive(Clone, Debug, Serialize)]
    struct TemplateHit {
        kind: String,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        score: f32,
    }

    impl TemplateHit {
        fn rect(&self) -> Rect {
            Rect::new(self.x, self.y, self.width, self.height)
        }

        fn center(&self) -> Point {
            self.rect().center()
        }
    }

    #[derive(Clone, Debug, Serialize)]
    struct ChatMessage {
        message_type: String,
        block: Rect,
        text: String,
    }

    #[derive(Clone, Debug)]
    struct HallInfo {
        name: String,
        remaining_minutes: Option<u32>,
    }

    #[derive(Clone, Debug)]
    struct HallInfoSample {
        name: String,
        time_text: String,
        remaining_minutes: Option<u32>,
    }

    #[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
    #[serde(rename_all = "snake_case")]
    enum UiStateKind {
        Primary,
        Secondary,
        Unknown,
    }

    #[derive(Clone, Debug, Serialize)]
    struct UiState {
        state: UiStateKind,
        blue_count: usize,
        yellow_count: usize,
        pink_count: usize,
        hall_visible: bool,
        enter_visible: bool,
        source: &'static str,
    }

    #[derive(Clone, Debug)]
    pub(super) struct ChangeFingerprint {
        pub(super) pixels: Vec<u8>,
        pub(super) width: u32,
        pub(super) height: u32,
    }

    #[derive(Clone, Copy, Debug)]
    pub(super) struct ChangeStats {
        pub(super) mean_abs_diff: f32,
        pub(super) changed_ratio: f32,
    }

    #[derive(Clone, Copy, Debug)]
    struct ChatMarkerCounts {
        blue: usize,
        yellow: usize,
        pink: usize,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum PlayOutcome {
        Success,
        NoSource,
        Error,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum UserDecision {
        Confirm,
        Skip,
        SwitchSource,
        Ai,
        Timeout,
        Stopped,
        PromptFailed,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct StderrLogger;

    impl Log for StderrLogger {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            metadata.level() <= log::max_level()
        }

        fn log(&self, record: &Record<'_>) {
            if self.enabled(record.metadata()) {
                eprintln!(
                    "{} {}",
                    logger::format_prefix(record.level()),
                    record.args()
                );
            }
        }

        fn flush(&self) {}
    }

    static STDERR_LOGGER: StderrLogger = StderrLogger;
    static RGB_TEMPLATE_CACHE: OnceLock<Mutex<HashMap<PathBuf, RgbImage>>> = OnceLock::new();

    impl UiState {
        fn primary_enter() -> Self {
            Self {
                state: UiStateKind::Primary,
                blue_count: 0,
                yellow_count: 0,
                pink_count: 0,
                hall_visible: false,
                enter_visible: true,
                source: "enter",
            }
        }

        fn primary_marker(blue_count: usize, yellow_count: usize, pink_count: usize) -> Self {
            Self {
                state: UiStateKind::Primary,
                blue_count,
                yellow_count,
                pink_count,
                hall_visible: false,
                enter_visible: false,
                source: "marker",
            }
        }

        fn secondary_hall() -> Self {
            Self {
                state: UiStateKind::Secondary,
                blue_count: 0,
                yellow_count: 0,
                pink_count: 0,
                hall_visible: true,
                enter_visible: false,
                source: "hall",
            }
        }

        fn unknown() -> Self {
            Self {
                state: UiStateKind::Unknown,
                blue_count: 0,
                yellow_count: 0,
                pink_count: 0,
                hall_visible: false,
                enter_visible: false,
                source: "none",
            }
        }

        fn is_primary(&self) -> bool {
            self.state == UiStateKind::Primary
        }
    }

    impl std::fmt::Display for UiState {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self.state {
                UiStateKind::Primary if self.source == "enter" => {
                    write!(formatter, "primary:enter")
                }
                UiStateKind::Primary => write!(
                    formatter,
                    "primary:marker blue={} yellow={} pink={}",
                    self.blue_count, self.yellow_count, self.pink_count
                ),
                UiStateKind::Secondary => write!(formatter, "secondary:hall"),
                UiStateKind::Unknown => write!(formatter, "unknown"),
            }
        }
    }

    pub fn run() -> Result<()> {
        dpi::set_process_dpi_awareness();
        let cli = Cli::parse();
        if cli.command.is_none() {
            return run_automation_with_watchdog(&cli.config, cli.watchdog_child);
        }
        let canvas = Canvas {
            width: cli.canvas.canvas_width,
            height: cli.canvas.canvas_height,
            resize: !cli.canvas.no_resize_canvas,
        };

        match cli.command.expect("checked command") {
            Command::Run => run_automation_with_watchdog(&cli.config, cli.watchdog_child),
            Command::Manual => manual_tools::run(&cli.config),
            Command::OcrImage { ocr, image } => {
                init_stderr_logger_once();
                let config = AppConfig::load_or_create(&cli.config)?;
                let ocr = ocr.resolve(&config);
                let engine = make_ocr_engine(&ocr)?;
                let image = image::open(&image)
                    .with_context(|| format!("open image {}", image.display()))?;
                let results = recognize_lines(&engine, &image)?;
                print_json(&results)
            }
            Command::OcrRegion { frame, ocr, rect } => {
                init_stderr_logger_once();
                let config = AppConfig::load_or_create(&cli.config)?;
                let ocr = ocr.resolve(&config);
                let engine = make_ocr_engine(&ocr)?;
                let frame = load_frame(&frame, &canvas, &config.window)?;
                let rect = parse_rect(&rect)?;
                let crop = crop_canvas(&frame.image, rect)?;
                let results = recognize_lines(&engine, &crop)?;
                print_json(&results)
            }
            Command::ScanChat {
                frame,
                ocr,
                templates,
            } => {
                init_stderr_logger_once();
                let config = AppConfig::load_or_create(&cli.config)?;
                let ocr = ocr.resolve(&config);
                let templates = templates.resolve(&config);
                let engine = make_ocr_engine(&ocr)?;
                let frame = load_frame(&frame, &canvas, &config.window)?;
                let started = Instant::now();
                let messages = scan_chat(
                    &frame.image,
                    &engine,
                    &templates,
                    config.screen.chat_rect.into(),
                )?;
                eprintln!("聊天区扫描耗时: {}ms", elapsed_ms(started));
                print_json(&messages)
            }
            Command::UiState { frame, templates } => {
                init_stderr_logger_once();
                let config = AppConfig::load_or_create(&cli.config)?;
                let templates = templates.resolve(&config);
                let frame = load_frame(&frame, &canvas, &config.window)?;
                let state = detect_ui_state(&frame.image, &templates, &config.screen)?;
                println!("{}", state);
                Ok(())
            }
            Command::HallName { frame, ocr } => {
                init_stderr_logger_once();
                let config = AppConfig::load_or_create(&cli.config)?;
                let ocr = ocr.resolve(&config);
                let engine = make_ocr_engine(&ocr)?;
                let frame = load_frame(&frame, &canvas, &config.window)?;
                let crop = crop_canvas(&frame.image, config.screen.hall_name_rect.into())?;
                let text = merged_ocr_text(&engine, &crop, config.ocr.same_line_y_tolerance)?;
                println!("{}", text);
                Ok(())
            }
            Command::MatchTemplate {
                frame,
                template,
                rect,
                threshold,
            } => {
                init_stderr_logger_once();
                let config = AppConfig::load_or_create(&cli.config)?;
                let frame = load_frame(&frame, &canvas, &config.window)?;
                let threshold = threshold.unwrap_or(config.templates.marker_threshold);
                let search_rect = match rect {
                    Some(value) => Some(parse_rect(&value)?),
                    None => None,
                };
                let started = Instant::now();
                let hits = find_template_hits(&frame.image, search_rect, &template, threshold)?;
                eprintln!("模板匹配耗时: {}ms", elapsed_ms(started));
                print_json(&hits)
            }
            Command::ClickTemplate {
                frame,
                template,
                rect,
                threshold,
                execute,
            } => {
                init_stderr_logger_once();
                let config = AppConfig::load_or_create(&cli.config)?;
                let frame = load_frame(&frame, &canvas, &config.window)?;
                let threshold = threshold.unwrap_or(config.templates.marker_threshold);
                let rect = parse_rect(&rect)?;
                let started = Instant::now();
                let hit = best_template_hit(&frame.image, Some(rect), &template, threshold)?
                    .ok_or_else(|| anyhow!("template not found above threshold"))?;
                eprintln!("模板匹配耗时: {}ms", elapsed_ms(started));
                let point = hit.center();
                run_or_print(execute, format!("click {},{}", point.x, point.y), || {
                    let config = AppConfig::load_or_create(&cli.config)?;
                    click_game_point(PointConfig::new(point.x, point.y), &config.window)
                })
            }
            Command::Click { x, y, execute } => {
                init_stderr_logger_once();
                run_or_print(execute, format!("click {},{}", x, y), || {
                    let config = AppConfig::load_or_create(&cli.config)?;
                    click_game_point(PointConfig::new(x, y), &config.window)
                })
            }
            Command::Key { key, execute } => {
                init_stderr_logger_once();
                let key = parse_key(&key)?;
                run_or_print(execute, format!("key {:?}", key), || {
                    let config = AppConfig::load_or_create(&cli.config)?;
                    press_key(key, &config.window)
                })
            }
            Command::SendChat { message, execute } => {
                run_or_print(execute, format!("send chat message: {}", message), || {
                    let config = AppConfig::load_or_create(&cli.config)?;
                    let output = ChatOutput::new(&config.output, &config.timing, &config.window);
                    output.send(&message)
                })
            }
        }
    }

    fn init_stderr_logger_once() {
        let _ = init_stderr_logger(LevelFilter::Info);
    }

    fn init_stderr_logger(level: LevelFilter) -> std::result::Result<(), SetLoggerError> {
        log::set_logger(&STDERR_LOGGER)?;
        log::set_max_level(level);
        Ok(())
    }

    fn run_automation_with_watchdog(config_path: &Path, watchdog_child: bool) -> Result<()> {
        if watchdog_child {
            return run_automation(config_path);
        }

        loop {
            let current_exe = std::env::current_exe().context("locate current executable")?;
            let mut child = ProcessCommand::new(&current_exe)
                .arg("--watchdog-child")
                .arg("--config")
                .arg(config_path)
                .arg("run")
                .spawn()
                .with_context(|| format!("启动监听子进程失败: {}", current_exe.display()))?;
            let status = child.wait().context("等待监听子进程退出")?;
            if status.success() {
                return Ok(());
            }

            let config = AppConfig::load_or_create(config_path)?;
            eprintln!(
                "监听子进程异常退出: status={}，{}ms 后重启",
                status, config.timing.watchdog_restart_ms
            );
            sleep(Duration::from_millis(config.timing.watchdog_restart_ms));
        }
    }

    fn run_automation(config_path: &Path) -> Result<()> {
        let config = AppConfig::load_or_create(config_path)?;
        let log_path = logger::init(&config.logging.dir, &config.logging.level)?;
        log::info!("日志文件: {}", log_path.display());
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
        log::info!(
            "OCR worker 内存重建阈值: {} bytes",
            config.ocr.memory_rebuild_limit_bytes
        );

        let runtime_state = PersistentRuntimeState::load(config.state.runtime_state_path.clone())?;
        let queue = PersistentQueue::load(config.state.queue_path.clone(), config.queue.max_size)?;
        log::info!("已加载队列: {} 首", queue.len());
        log::info!(
            "已加载运行时状态: paused_by_command={}",
            runtime_state.state().paused_by_command
        );

        let mut app = AutomationApp::new(config, runtime_state, queue)?;
        app.run()
    }

    struct AutomationApp {
        config: AppConfig,
        runtime_state: Arc<Mutex<PersistentRuntimeState>>,
        queue: Arc<Mutex<PersistentQueue>>,
        feeluown: FeelUOwnClient,
        ai: ai::AiClient,
        chat_output: ChatOutput,
        ocr_engine: Arc<Mutex<OcrEngineState>>,
        locks: CommandLockState,
        pending: Arc<(Mutex<VecDeque<PendingTask>>, Condvar)>,
        screen_lock_primed: Arc<AtomicBool>,
        reset_locks_requested: Arc<AtomicBool>,
        invite_executed_seqs: Arc<Mutex<HashSet<u32>>>,
        commands_enabled: Arc<AtomicBool>,
        idle_exit: Arc<Mutex<Option<IdleExitState>>>,
        running: Arc<AtomicBool>,
        paused: Arc<AtomicBool>,
        command_executing: Arc<AtomicBool>,
        song_command_executing: Arc<AtomicBool>,
    }

    #[derive(Clone, Debug)]
    struct IdleExitState {
        timeout: Duration,
        last_command_at: Instant,
    }

    struct OcrEngineState {
        engine: OcrEngine,
        rebuild_due_at: Instant,
    }

    #[derive(Clone, Debug)]
    struct PlaybackSnapshot {
        status: PlayerStatus,
        captured_at: Instant,
    }

    #[derive(Clone, Debug)]
    struct ResolvedSongRequest {
        keyword: String,
        source: String,
        prefer_accompaniment: bool,
        ai_original_text: String,
        uri: String,
        skip_match_check: bool,
        friend_username: String,
    }

    impl ResolvedSongRequest {
        fn match_keyword(&self) -> &str {
            if self.ai_original_text.trim().is_empty() {
                &self.keyword
            } else {
                &self.ai_original_text
            }
        }
    }

    enum PendingTask {
        Command(Box<PendingCommand>),
        AdvanceQueue { reason: &'static str },
    }

    impl PendingTask {
        fn label(&self) -> String {
            match self {
                Self::Command(pending) => pending.parsed.raw.clone(),
                Self::AdvanceQueue { reason } => format!("自动出队({})", reason),
            }
        }

        fn same_lock_command(&self, parsed: &ParsedCommand) -> bool {
            match self {
                Self::Command(pending) => command::same_lock_command(&pending.parsed, parsed),
                Self::AdvanceQueue { .. } => false,
            }
        }
    }

    struct CommandExecutingGuard {
        flag: Arc<AtomicBool>,
    }

    impl CommandExecutingGuard {
        fn new(flag: Arc<AtomicBool>) -> Self {
            flag.store(true, AtomicOrdering::SeqCst);
            Self { flag }
        }
    }

    impl Drop for CommandExecutingGuard {
        fn drop(&mut self) {
            self.flag.store(false, AtomicOrdering::SeqCst);
        }
    }

    struct SongCommandExecutingGuard {
        flag: Arc<AtomicBool>,
    }

    impl SongCommandExecutingGuard {
        fn new(flag: Arc<AtomicBool>) -> Self {
            flag.store(true, AtomicOrdering::SeqCst);
            Self { flag }
        }
    }

    impl Drop for SongCommandExecutingGuard {
        fn drop(&mut self) {
            self.flag.store(false, AtomicOrdering::SeqCst);
        }
    }

    impl AutomationApp {
        fn new(
            config: AppConfig,
            runtime_state: PersistentRuntimeState,
            queue: PersistentQueue,
        ) -> Result<Self> {
            let ocr_args = OcrArgs::default().resolve(&config);
            let ocr_engine = make_ocr_engine(&ocr_args)?;
            let feeluown = FeelUOwnClient::new(&config.feeluown, &config.timing);
            let ai = ai::AiClient::new(&config.ai, &config.timing);
            let chat_output = ChatOutput::new(&config.output, &config.timing, &config.window);
            Ok(Self {
                config,
                runtime_state: Arc::new(Mutex::new(runtime_state)),
                queue: Arc::new(Mutex::new(queue)),
                feeluown,
                ai,
                chat_output,
                ocr_engine: Arc::new(Mutex::new(OcrEngineState {
                    engine: ocr_engine,
                    rebuild_due_at: Instant::now() + OCR_REBUILD_INTERVAL,
                })),
                locks: CommandLockState::default(),
                pending: Arc::new((Mutex::new(VecDeque::new()), Condvar::new())),
                screen_lock_primed: Arc::new(AtomicBool::new(false)),
                reset_locks_requested: Arc::new(AtomicBool::new(false)),
                invite_executed_seqs: Arc::new(Mutex::new(HashSet::new())),
                commands_enabled: Arc::new(AtomicBool::new(true)),
                idle_exit: Arc::new(Mutex::new(None)),
                running: Arc::new(AtomicBool::new(true)),
                paused: Arc::new(AtomicBool::new(false)),
                command_executing: Arc::new(AtomicBool::new(false)),
                song_command_executing: Arc::new(AtomicBool::new(false)),
            })
        }

        fn run(&mut self) -> Result<()> {
            self.warn_if_screen_size_mismatch()?;
            self.start_http_server()?;
            self.start_hotkeys()?;
            let executor = self.start_command_executor();
            let playback_monitor = self.start_playback_monitor();
            let result = self.run_scan_loop();
            self.running.store(false, AtomicOrdering::SeqCst);
            self.notify_pending_executor();
            if let Err(error) = executor.join() {
                log::error!("命令执行线程 panic: {error:?}");
            }
            if let Err(error) = playback_monitor.join() {
                log::error!("播放监控线程 panic: {error:?}");
            }
            if let Err(error) = self.queue().and_then(|queue| queue.save()) {
                log::error!("退出前保存队列失败: {error:#}");
            }
            if let Err(error) = self.runtime_state().and_then(|state| state.save()) {
                log::error!("退出前保存运行状态失败: {error:#}");
            }
            result
        }

        fn start_command_executor(&self) -> thread::JoinHandle<()> {
            let mut executor = Self {
                config: self.config.clone(),
                runtime_state: self.runtime_state.clone(),
                queue: self.queue.clone(),
                feeluown: self.feeluown.clone(),
                ai: self.ai.clone(),
                chat_output: self.chat_output.clone(),
                ocr_engine: self.ocr_engine.clone(),
                locks: CommandLockState::default(),
                pending: self.pending.clone(),
                screen_lock_primed: self.screen_lock_primed.clone(),
                reset_locks_requested: self.reset_locks_requested.clone(),
                invite_executed_seqs: self.invite_executed_seqs.clone(),
                commands_enabled: self.commands_enabled.clone(),
                idle_exit: self.idle_exit.clone(),
                running: self.running.clone(),
                paused: self.paused.clone(),
                command_executing: self.command_executing.clone(),
                song_command_executing: self.song_command_executing.clone(),
            };
            thread::spawn(move || {
                log::info!("命令执行线程已启动");
                if let Err(error) = executor.run_pending_command_loop() {
                    log::error!("命令执行线程异常退出: {error:#}");
                }
            })
        }

        fn start_playback_monitor(&self) -> thread::JoinHandle<()> {
            let mut monitor = Self {
                config: self.config.clone(),
                runtime_state: self.runtime_state.clone(),
                queue: self.queue.clone(),
                feeluown: self.feeluown.clone(),
                ai: self.ai.clone(),
                chat_output: self.chat_output.clone(),
                ocr_engine: self.ocr_engine.clone(),
                locks: CommandLockState::default(),
                pending: self.pending.clone(),
                screen_lock_primed: self.screen_lock_primed.clone(),
                reset_locks_requested: self.reset_locks_requested.clone(),
                invite_executed_seqs: self.invite_executed_seqs.clone(),
                commands_enabled: self.commands_enabled.clone(),
                idle_exit: self.idle_exit.clone(),
                running: self.running.clone(),
                paused: self.paused.clone(),
                command_executing: self.command_executing.clone(),
                song_command_executing: self.song_command_executing.clone(),
            };
            thread::spawn(move || {
                log::info!("播放监控线程已启动");
                monitor.run_playback_monitor_loop();
            })
        }

        fn notify_pending_executor(&self) {
            let (_, cvar) = &*self.pending;
            cvar.notify_all();
        }

        fn queue(&self) -> Result<MutexGuard<'_, PersistentQueue>> {
            self.queue
                .lock()
                .map_err(|_| anyhow!("queue mutex poisoned"))
        }

        fn runtime_state(&self) -> Result<MutexGuard<'_, PersistentRuntimeState>> {
            self.runtime_state
                .lock()
                .map_err(|_| anyhow!("runtime state mutex poisoned"))
        }

        fn ocr_engine(&self) -> Result<MutexGuard<'_, OcrEngineState>> {
            let mut guard = self
                .ocr_engine
                .lock()
                .map_err(|_| anyhow!("ocr_engine mutex poisoned"))?;
            if Instant::now() >= guard.rebuild_due_at {
                log::info!("OCR 引擎运行超过 1 小时，开始重建");
                let started = Instant::now();
                let ocr_args = OcrArgs::default().resolve(&self.config);
                match make_ocr_engine(&ocr_args) {
                    Ok(engine) => {
                        guard.engine = engine;
                        guard.rebuild_due_at = Instant::now() + OCR_REBUILD_INTERVAL;
                        log::info!("OCR 引擎重建完成: {}ms", elapsed_ms(started));
                    }
                    Err(error) => {
                        guard.rebuild_due_at = Instant::now() + OCR_REBUILD_RETRY_INTERVAL;
                        log::error!("OCR 引擎重建失败，继续使用旧引擎，5分钟后重试: {error:#}");
                    }
                }
            }
            Ok(guard)
        }

        fn warn_if_screen_size_mismatch(&self) -> Result<()> {
            let frame = window::capture_game(&self.config.window)?;
            if self.config.screen.warn_on_size_mismatch
                && (frame.width() != self.config.screen.expected_width
                    || frame.height() != self.config.screen.expected_height)
            {
                log::warn!(
                    "截图尺寸为 {}x{}，预期 {}x{}，程序继续运行",
                    frame.width(),
                    frame.height(),
                    self.config.screen.expected_width,
                    self.config.screen.expected_height
                );
            }
            Ok(())
        }

        fn start_http_server(&self) -> Result<()> {
            if !self.config.http.enabled {
                return Ok(());
            }
            http_server::start(http_server::HttpSharedState::new(
                self.config.clone(),
                Arc::clone(&self.queue),
                Arc::clone(&self.runtime_state),
            ))
        }

        fn start_hotkeys(&self) -> Result<()> {
            hotkeys::start(
                &self.config.hotkeys,
                Arc::clone(&self.running),
                Arc::clone(&self.paused),
            )
        }

        fn run_scan_loop(&mut self) -> Result<()> {
            let template_args = TemplateArgs::default().resolve(&self.config);
            let ui_template_args = UiTemplateArgs::default().resolve(&self.config);
            let canvas = Canvas {
                width: self.config.screen.expected_width,
                height: self.config.screen.expected_height,
                resize: true,
            };
            let frame_args = FrameArgs { image: None };
            let mut last_fingerprint: Option<ChangeFingerprint> = None;
            let mut last_ocr_at =
                Instant::now() - Duration::from_millis(self.config.timing.chat_scan_fallback_ms);
            let mut last_change_ocr_at =
                Instant::now() - Duration::from_millis(self.config.timing.chat_change_cooldown_ms);
            let mut suppress_change_until = Instant::now();
            let mut force_scan_after: Option<Instant> = None;
            let mut force_scan_reason: Option<&'static str> = None;
            let mut primary_visible = false;

            log::info!("自动化扫描已启动");
            while self.running.load(AtomicOrdering::SeqCst) {
                if self.paused.load(AtomicOrdering::SeqCst) {
                    self.maybe_idle_exit()?;
                    sleep(Duration::from_millis(self.config.timing.scan_loop_idle_ms));
                    continue;
                }

                match load_frame(&frame_args, &canvas, &self.config.window) {
                    Ok(frame) => {
                        match detect_ui_state(&frame.image, &ui_template_args, &self.config.screen)
                        {
                            Ok(ui_state) if ui_state.is_primary() => {
                                let entered_primary = !primary_visible;
                                primary_visible = true;
                                let fingerprint = match chat_change_fingerprint(
                                    &frame.image,
                                    self.config.screen.chat_rect.into(),
                                ) {
                                    Ok(fingerprint) => Some(fingerprint),
                                    Err(error) => {
                                        log::error!("聊天区变化指纹失败: {error:#}");
                                        None
                                    }
                                };
                                let now = Instant::now();
                                if entered_primary {
                                    if let Some(fingerprint) = fingerprint.clone() {
                                        last_fingerprint = Some(fingerprint);
                                        let scan_after = now
                                            + Duration::from_millis(
                                                self.config.timing.chat_change_debounce_ms,
                                            );
                                        if force_scan_after.is_none_or(|time| scan_after < time) {
                                            force_scan_after = Some(scan_after);
                                            force_scan_reason = Some("enter-primary");
                                        }
                                        log::debug!(
                                            "进入一级界面，已建立聊天区对比基线，快速扫描延迟={}ms",
                                            self.config.timing.chat_change_debounce_ms
                                        );
                                    }
                                }
                                let change_suppressed = now < suppress_change_until;
                                let forced_scan_due =
                                    force_scan_after.is_some_and(|time| now >= time);
                                let cooldown_until = last_change_ocr_at
                                    + Duration::from_millis(
                                        self.config.timing.chat_change_cooldown_ms,
                                    );
                                let change_stats = fingerprint.as_ref().and_then(|current| {
                                    last_fingerprint
                                        .as_ref()
                                        .map(|previous| change_stats(previous, current))
                                });
                                let change_over_threshold = change_stats.is_some_and(|stats| {
                                    stats.mean_abs_diff >= self.config.ocr.change_mean_threshold
                                        || stats.changed_ratio
                                            >= self.config.ocr.change_pixel_threshold
                                });
                                let change_ready = !change_suppressed && now >= cooldown_until;
                                let mut keep_previous_fingerprint = false;
                                if change_over_threshold && !change_ready && !forced_scan_due {
                                    let scan_after = if change_suppressed {
                                        suppress_change_until
                                    } else {
                                        cooldown_until
                                    };
                                    if force_scan_after.is_none_or(|time| scan_after < time) {
                                        force_scan_after = Some(scan_after);
                                        force_scan_reason = Some("delayed-change");
                                    }
                                    keep_previous_fingerprint = true;
                                }
                                let fallback_due = !change_suppressed
                                    && (forced_scan_due
                                        || now.duration_since(last_ocr_at)
                                            >= Duration::from_millis(
                                                self.config.timing.chat_scan_fallback_ms,
                                            ));
                                let change_due = change_over_threshold && change_ready;

                                let mut scanned_this_round = false;
                                if change_due {
                                    let stats = change_stats.expect("change_due requires stats");
                                    log::info!(
                                        "触发聊天扫描: reason=change mean={:.3} ratio={:.5} debounce={}ms",
                                        stats.mean_abs_diff,
                                        stats.changed_ratio,
                                        self.config.timing.chat_change_debounce_ms
                                    );
                                    sleep(Duration::from_millis(
                                        self.config.timing.chat_change_debounce_ms,
                                    ));
                                    match load_frame(&frame_args, &canvas, &self.config.window) {
                                        Ok(frame) => {
                                            let messages = {
                                                let engine = self.ocr_engine()?;
                                                scan_chat(
                                                    &frame.image,
                                                    &engine.engine,
                                                    &template_args,
                                                    self.config.screen.chat_rect.into(),
                                                )
                                            };
                                            match messages {
                                                Ok(messages) => {
                                                    self.handle_scan_messages(messages)?
                                                }
                                                Err(error) => {
                                                    log::error!("聊天扫描失败: {error:#}")
                                                }
                                            }
                                            last_ocr_at = Instant::now();
                                            last_change_ocr_at = last_ocr_at;
                                            force_scan_after = None;
                                            force_scan_reason = None;
                                            last_fingerprint = chat_change_fingerprint(
                                                &frame.image,
                                                self.config.screen.chat_rect.into(),
                                            )
                                            .ok();
                                            scanned_this_round = true;
                                        }
                                        Err(error) => log::error!("变化后截图失败: {error:#}"),
                                    }
                                } else if fallback_due {
                                    let reason = if forced_scan_due {
                                        force_scan_reason.unwrap_or("forced")
                                    } else {
                                        "poll"
                                    };
                                    log::info!(
                                        "触发聊天扫描: reason={} since_last={}ms",
                                        reason,
                                        now.duration_since(last_ocr_at).as_millis()
                                    );
                                    let messages = {
                                        let engine = self.ocr_engine()?;
                                        scan_chat(
                                            &frame.image,
                                            &engine.engine,
                                            &template_args,
                                            self.config.screen.chat_rect.into(),
                                        )
                                    };
                                    match messages {
                                        Ok(messages) => self.handle_scan_messages(messages)?,
                                        Err(error) => log::error!("聊天扫描失败: {error:#}"),
                                    }
                                    last_ocr_at = now;
                                    force_scan_after = None;
                                    force_scan_reason = None;
                                    last_fingerprint = fingerprint.clone();
                                    scanned_this_round = true;
                                }

                                if change_suppressed {
                                    last_fingerprint = None;
                                } else if !scanned_this_round
                                    && !keep_previous_fingerprint
                                    && last_fingerprint.is_none()
                                {
                                    // 不要每帧滚动更新基线，慢速聊天动画会在超过阈值前被吃掉。
                                    if let Some(fingerprint) = fingerprint {
                                        last_fingerprint = Some(fingerprint);
                                    }
                                }
                            }
                            Ok(ui_state) => {
                                primary_visible = false;
                                log::debug!("当前不是一级聊天界面，跳过聊天扫描: {}", ui_state);
                                last_fingerprint = None;
                            }
                            Err(error) => {
                                primary_visible = false;
                                log::error!("界面状态检测失败: {error:#}");
                            }
                        }
                    }
                    Err(error) => {
                        primary_visible = false;
                        log::error!("截图失败: {error:#}");
                    }
                }
                if primary_visible && self.maybe_warn_hall_expiring()? {
                    suppress_change_until = Instant::now()
                        + Duration::from_millis(self.config.timing.post_command_settle_ms);
                    force_scan_after = Some(suppress_change_until);
                    force_scan_reason = Some("hall-expiring");
                    last_fingerprint = None;
                    last_ocr_at = Instant::now();
                }
                self.maybe_idle_exit()?;
                sleep(Duration::from_millis(self.config.timing.scan_loop_idle_ms));
            }

            self.queue()?.save()?;
            self.runtime_state()?.save()?;
            Ok(())
        }

        fn handle_scan_messages(&mut self, messages: Vec<ChatMessage>) -> Result<()> {
            if self
                .reset_locks_requested
                .swap(false, AtomicOrdering::SeqCst)
            {
                self.locks = CommandLockState::default();
                log::info!("已重置命令屏幕锁");
            }
            if messages.is_empty() {
                log::debug!("没有找到聊天标志，本轮不更新命令锁");
                return Ok(());
            }

            let mut parsed = Vec::new();
            for message in messages.iter().filter(|message| !message.text.is_empty()) {
                log::debug!("识别文本: [{}] {}", message.message_type, message.text);
                let Some(parsed_command) =
                    command::parse_text(&message.text, &message.message_type)
                else {
                    continue;
                };
                if !self.commands_enabled.load(AtomicOrdering::SeqCst)
                    && message.message_type != "pink"
                {
                    log::info!("命令识别已禁用，跳过: {}", parsed_command.raw);
                    continue;
                }
                if let UserCommand::Invite(invite) = &parsed_command.command {
                    let invite_executed = self
                        .invite_executed_seqs
                        .lock()
                        .map_err(|_| anyhow!("invite_executed_seqs mutex poisoned"))?
                        .contains(&invite.seq);
                    if invite_executed {
                        log::info!(
                            "邀请参数 {} 已执行过，跳过: {}",
                            invite.seq,
                            parsed_command.raw
                        );
                        continue;
                    }
                }
                if parsed
                    .iter()
                    .any(|existing| command::same_lock_command(existing, &parsed_command))
                {
                    log::info!("同轮重复识别命令，已合并: {}", parsed_command.raw);
                    continue;
                }
                log::debug!("解析命令: {}", parsed_command.raw);
                parsed.push(parsed_command);
            }

            let update = self
                .locks
                .update(&parsed, self.command_executing.load(AtomicOrdering::SeqCst));
            for command in update.unlocked {
                log::info!("解锁: {}", command);
            }
            for command in update.skipped {
                log::info!("命令仍在屏幕内，本轮跳过: {}", command);
            }
            if !self.screen_lock_primed.swap(true, AtomicOrdering::SeqCst) {
                for pending in update.accepted {
                    log::info!(
                        "启动屏幕锁已记录当前可见命令，不执行: {}",
                        pending.parsed.raw
                    );
                }
                return Ok(());
            }
            for pending in update.accepted {
                if self.pending_contains_command(&pending.parsed)? {
                    log::info!("命令已在待处理队列，本轮跳过: {}", pending.parsed.raw);
                    continue;
                }
                match &pending.parsed.command {
                    UserCommand::DisableCommands { username: _ } => {
                        self.commands_enabled.store(false, AtomicOrdering::SeqCst);
                    }
                    UserCommand::EnableCommands { username: _ } => {
                        self.commands_enabled.store(true, AtomicOrdering::SeqCst);
                    }
                    UserCommand::IdleExit { minutes } => {
                        self.record_command_activity()?;
                        self.configure_idle_exit(*minutes)?;
                        if let Err(error) = self.log_executed_command(
                            &pending.parsed,
                            &format!("idle exit {}", minutes),
                        ) {
                            log::error!("写入执行命令日志失败: {error:#}");
                        }
                        continue;
                    }
                    _ => {}
                }
                log::info!("命令已加入待处理队列: {}", pending.parsed.raw);
                self.record_command_activity()?;
                self.push_pending_task(PendingTask::Command(Box::new(pending)))?;
            }
            Ok(())
        }

        fn record_command_activity(&self) -> Result<()> {
            let mut state = self
                .idle_exit
                .lock()
                .map_err(|_| anyhow!("idle_exit mutex poisoned"))?;
            if let Some(state) = state.as_mut() {
                state.last_command_at = Instant::now();
            }
            Ok(())
        }

        fn maybe_idle_exit(&self) -> Result<()> {
            let Some(timeout) = self.idle_exit_due()? else {
                return Ok(());
            };
            if !self.executor_is_idle()? {
                return Ok(());
            }
            log::info!("闲置退出触发: {}分钟无新命令", timeout.as_secs() / 60);
            if let Err(error) = window::close_game(&self.config.window) {
                log::error!("关闭目标窗口失败: {error:#}");
            }
            self.running.store(false, AtomicOrdering::SeqCst);
            self.notify_pending_executor();
            Ok(())
        }

        fn idle_exit_due(&self) -> Result<Option<Duration>> {
            let state = self
                .idle_exit
                .lock()
                .map_err(|_| anyhow!("idle_exit mutex poisoned"))?;
            let Some(state) = state.as_ref() else {
                return Ok(None);
            };
            if state.last_command_at.elapsed() >= state.timeout {
                Ok(Some(state.timeout))
            } else {
                Ok(None)
            }
        }

        fn run_pending_command_loop(&mut self) -> Result<()> {
            while self.running.load(AtomicOrdering::SeqCst) {
                if self.paused.load(AtomicOrdering::SeqCst) {
                    sleep(Duration::from_millis(self.config.timing.scan_loop_idle_ms));
                    continue;
                }
                let Some((task, executing)) = self.wait_for_pending_task()? else {
                    continue;
                };
                if self.paused.load(AtomicOrdering::SeqCst) {
                    self.push_pending_task_front(task)?;
                    drop(executing);
                    sleep(Duration::from_millis(self.config.timing.scan_loop_idle_ms));
                    continue;
                }
                match self.execute_pending_task(task) {
                    Ok(true) => {
                        sleep(Duration::from_millis(
                            self.config.timing.post_command_settle_ms,
                        ));
                    }
                    Ok(false) => {}
                    Err(error) => {
                        log::error!("待处理任务执行异常: {error:#}");
                    }
                }
            }
            Ok(())
        }

        fn wait_for_pending_task(&self) -> Result<Option<(PendingTask, CommandExecutingGuard)>> {
            let (lock, cvar) = &*self.pending;
            let mut guard = lock.lock().map_err(|_| anyhow!("pending mutex poisoned"))?;
            while guard.is_empty() && self.running.load(AtomicOrdering::SeqCst) {
                guard = cvar
                    .wait_timeout(guard, Duration::from_secs(1))
                    .map_err(|_| anyhow!("pending condvar poisoned"))?
                    .0;
            }
            if !self.running.load(AtomicOrdering::SeqCst) {
                return Ok(None);
            }
            let executing = CommandExecutingGuard::new(Arc::clone(&self.command_executing));
            Ok(guard.pop_front().map(|task| {
                log::info!("待处理任务开始: {}", task.label());
                (task, executing)
            }))
        }

        fn execute_pending_task(&mut self, task: PendingTask) -> Result<bool> {
            let label = task.label();
            let result = match task {
                PendingTask::Command(pending) => {
                    let _song_command_guard =
                        if matches!(&pending.parsed.command, UserCommand::Song(_)) {
                            Some(SongCommandExecutingGuard::new(Arc::clone(
                                &self.song_command_executing,
                            )))
                        } else {
                            None
                        };
                    self.execute_pending_command(*pending)
                }
                PendingTask::AdvanceQueue { reason } => self.consume_queue(reason),
            };
            match result {
                Ok(()) => {
                    log::info!("待处理任务完成: {}", label);
                    Ok(true)
                }
                Err(error) => {
                    log::error!("待处理任务失败 {}: {error:#}", label);
                    self.return_to_primary_after_command_failure(&label);
                    Err(error)
                }
            }
        }

        fn execute_pending_command(&mut self, pending: PendingCommand) -> Result<()> {
            match self.prepare_command_ui(&pending.parsed.raw) {
                Ok(true) => {}
                Ok(false) => {
                    log::info!(
                        "命令执行前未能回到一级界面，保留待处理命令: {}",
                        pending.parsed.raw
                    );
                    self.push_pending_task_front(PendingTask::Command(Box::new(pending)))?;
                    return Ok(());
                }
                Err(error) => {
                    log::error!(
                        "命令执行前准备界面失败，保留待处理命令 {}: {error:#}",
                        pending.parsed.raw
                    );
                    self.push_pending_task_front(PendingTask::Command(Box::new(pending)))?;
                    return Ok(());
                }
            }
            log::info!(
                "执行待处理命令: {} lock={}",
                pending.parsed.raw,
                pending.lock_key
            );
            let command_started = Instant::now();
            match self.execute_command(&pending.parsed) {
                Ok(()) => log::info!(
                    "命令执行完成: {} 耗时={}ms",
                    pending.parsed.raw,
                    elapsed_ms(command_started)
                ),
                Err(error) => {
                    log::error!(
                        "命令执行失败 {} 耗时={}ms: {error:#}",
                        pending.parsed.raw,
                        elapsed_ms(command_started)
                    );
                    self.return_to_primary_after_command_failure(&pending.parsed.raw);
                }
            }
            Ok(())
        }

        fn log_executed_command(&self, parsed: &ParsedCommand, final_command: &str) -> Result<()> {
            let path = &self.config.state.executed_commands_log_path;
            if let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                fs::create_dir_all(parent).with_context(|| {
                    format!("create command log directory {}", parent.display())
                })?;
            }
            let line = format!(
                "{}-{}-{}-{}-{}\n",
                command_log_timestamp(),
                command_log_field(command_location(&parsed.message_type)),
                command_log_field(command_username(parsed)),
                command_log_field(&parsed.user_command),
                command_log_field(final_command),
            );
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("open command log {}", path.display()))?;
            file.write_all(line.as_bytes())
                .with_context(|| format!("write command log {}", path.display()))
        }

        fn pending_contains_command(&self, parsed: &ParsedCommand) -> Result<bool> {
            let (lock, _) = &*self.pending;
            let guard = lock.lock().map_err(|_| anyhow!("pending mutex poisoned"))?;
            Ok(guard.iter().any(|task| task.same_lock_command(parsed)))
        }

        fn executor_is_idle(&self) -> Result<bool> {
            let (lock, _) = &*self.pending;
            let guard = lock.lock().map_err(|_| anyhow!("pending mutex poisoned"))?;
            Ok(guard.is_empty() && !self.command_executing.load(AtomicOrdering::SeqCst))
        }

        fn push_pending_task(&self, task: PendingTask) -> Result<()> {
            let (lock, cvar) = &*self.pending;
            let mut guard = lock.lock().map_err(|_| anyhow!("pending mutex poisoned"))?;
            guard.push_back(task);
            cvar.notify_one();
            Ok(())
        }

        fn push_pending_task_front(&self, task: PendingTask) -> Result<()> {
            let (lock, cvar) = &*self.pending;
            let mut guard = lock.lock().map_err(|_| anyhow!("pending mutex poisoned"))?;
            guard.push_front(task);
            cvar.notify_one();
            Ok(())
        }

        fn prepare_command_ui(&self, command: &str) -> Result<bool> {
            let templates = UiTemplateArgs::default().resolve(&self.config);
            let canvas = Canvas {
                width: self.config.screen.expected_width,
                height: self.config.screen.expected_height,
                resize: true,
            };
            let frame_args = FrameArgs { image: None };
            let deadline =
                Instant::now() + Duration::from_millis(self.config.timing.command_ui_timeout_ms);

            while self.running.load(AtomicOrdering::SeqCst) && Instant::now() < deadline {
                let frame = load_frame(&frame_args, &canvas, &self.config.window)?;
                let ui_state = detect_ui_state(&frame.image, &templates, &self.config.screen)?;
                if ui_state.is_primary() {
                    log::info!("命令执行前界面: {}", ui_state);
                    return Ok(true);
                }

                log::info!("命令执行前界面: {}，按 ESC 返回一级: {}", ui_state, command);
                press_key(Key::Escape, &self.config.window)?;
                sleep(Duration::from_millis(
                    self.config.timing.return_to_primary_retry_ms,
                ));
            }

            Ok(false)
        }

        fn run_playback_monitor_loop(&mut self) {
            let tick_ms = self.config.timing.playback_monitor_tick_ms.max(50);
            let status_ms = self.config.timing.playback_monitor_status_ms.max(tick_ms);
            let mut snapshot: Option<PlaybackSnapshot> = None;
            let mut next_status_at = Instant::now();

            while self.running.load(AtomicOrdering::SeqCst) {
                if self.paused.load(AtomicOrdering::SeqCst) {
                    sleep(Duration::from_millis(tick_ms));
                    continue;
                }

                let now = Instant::now();
                if snapshot.is_none() || now >= next_status_at {
                    match self.feeluown.status() {
                        Ok(status) => {
                            snapshot = Some(PlaybackSnapshot {
                                status,
                                captured_at: now,
                            });
                            next_status_at = now + Duration::from_millis(status_ms);
                        }
                        Err(error) => {
                            log::error!("播放监控状态查询失败: {error:#}");
                            snapshot = None;
                            next_status_at = now + Duration::from_millis(status_ms);
                        }
                    }
                }

                if let Some(playback_snapshot) = snapshot.as_ref() {
                    match self.maybe_advance_queue(playback_snapshot) {
                        Ok(true) => {
                            if let Ok(status) = self.feeluown.status() {
                                snapshot = Some(PlaybackSnapshot {
                                    status,
                                    captured_at: Instant::now(),
                                });
                            } else {
                                snapshot = None;
                            }
                            next_status_at = Instant::now() + Duration::from_millis(status_ms);
                        }
                        Ok(false) => {}
                        Err(error) => {
                            log::error!("播放监控处理失败: {error:#}");
                            next_status_at = Instant::now() + Duration::from_millis(status_ms);
                        }
                    }
                }

                sleep(Duration::from_millis(tick_ms));
            }
        }

        fn maybe_advance_queue(&mut self, snapshot: &PlaybackSnapshot) -> Result<bool> {
            let status = estimated_player_status(snapshot);

            let current_song = format!("{}{}", status.name, status.singer);
            let runtime_snapshot = self.runtime_state()?.state().clone();
            if runtime_snapshot.current_song_is_requested
                && !current_song.is_empty()
                && !runtime_snapshot.last_requested_song.is_empty()
                && current_song != runtime_snapshot.last_requested_song
            {
                let mut runtime_state = self.runtime_state()?;
                runtime_state.state_mut().current_song_is_requested = false;
                runtime_state.state_mut().last_requested_song.clear();
                runtime_state.state_mut().last_requested_keyword.clear();
                runtime_state.state_mut().last_requested_source.clear();
                runtime_state
                    .state_mut()
                    .last_requested_prefer_accompaniment = false;
                runtime_state.save()?;
                log::info!("检测到歌曲已切换，取消点歌标记");
            }

            let queue_empty = self.queue()?.is_empty();
            let has_pending_task = self.has_pending_task()?;
            let command_executing = self.command_executing.load(AtomicOrdering::SeqCst);
            let song_command_executing = self.song_command_executing.load(AtomicOrdering::SeqCst);
            let has_pending_playback = !queue_empty || has_pending_task || song_command_executing;

            if queue_empty && !has_pending_task && !command_executing && !song_command_executing {
                return self.resume_pending_playback_pause_if_idle();
            }

            if status.status == "stopped" || status.status == "stoped" {
                if command_executing || has_pending_task || queue_empty {
                    return Ok(false);
                }
                let mut runtime_state = self.runtime_state()?;
                runtime_state.state_mut().paused_by_command = false;
                runtime_state.save()?;
                drop(runtime_state);
                self.push_pending_task(PendingTask::AdvanceQueue { reason: "停止" })?;
                return Ok(true);
            }
            if status.status == "paused" {
                if self.runtime_state()?.state().paused_by_command {
                    return Ok(false);
                }
                if self.runtime_state()?.state().paused_for_pending_playback {
                    if !command_executing && !has_pending_task && !queue_empty {
                        self.push_pending_task(PendingTask::AdvanceQueue {
                            reason: "即将结束"
                        })?;
                        return Ok(true);
                    }
                    return Ok(false);
                }
                if command_executing {
                    return Ok(false);
                }
                if has_pending_task {
                    return Ok(false);
                }
                if queue_empty {
                    return Ok(false);
                }
                let mut runtime_state = self.runtime_state()?;
                runtime_state.state_mut().paused_by_command = false;
                runtime_state.state_mut().paused_for_pending_playback = false;
                runtime_state.save()?;
                drop(runtime_state);
                self.push_pending_task(PendingTask::AdvanceQueue { reason: "暂停" })?;
                return Ok(true);
            }
            if status.status != "playing" {
                return Ok(false);
            }

            let should_clear_pause_flags = {
                let runtime_state = self.runtime_state()?;
                runtime_state.state().paused_by_command
                    || runtime_state.state().paused_for_pending_playback
            };
            if should_clear_pause_flags {
                let mut runtime_state = self.runtime_state()?;
                runtime_state.state_mut().paused_by_command = false;
                runtime_state.state_mut().paused_for_pending_playback = false;
                runtime_state.save()?;
            }
            if status.duration > 0.0 {
                let remaining = status.duration - status.progress;
                if remaining <= self.config.queue.auto_advance_seconds as f64
                    && has_pending_playback
                {
                    let paused = self.pause_for_pending_playback()?;
                    if !command_executing && !has_pending_task && !queue_empty {
                        self.push_pending_task(PendingTask::AdvanceQueue {
                            reason: "即将结束"
                        })?;
                        return Ok(true);
                    }
                    return Ok(paused);
                }
            }
            Ok(false)
        }

        fn has_pending_task(&self) -> Result<bool> {
            let (lock, _) = &*self.pending;
            let guard = lock.lock().map_err(|_| anyhow!("pending mutex poisoned"))?;
            Ok(!guard.is_empty())
        }

        fn pause_for_pending_playback(&mut self) -> Result<bool> {
            if self.runtime_state()?.state().paused_for_pending_playback {
                return Ok(false);
            }
            log::info!("歌曲即将结束，暂停等待点歌或队列播放");
            self.feeluown.pause()?;
            let mut runtime_state = self.runtime_state()?;
            runtime_state.state_mut().paused_for_pending_playback = true;
            runtime_state.state_mut().paused_by_command = false;
            runtime_state.save()?;
            Ok(true)
        }

        fn resume_pending_playback_pause_if_idle(&mut self) -> Result<bool> {
            if !self.runtime_state()?.state().paused_for_pending_playback {
                return Ok(false);
            }
            log::info!("没有待执行点歌或队列，恢复临近结束暂停的播放");
            self.feeluown.play()?;
            let mut runtime_state = self.runtime_state()?;
            runtime_state.state_mut().paused_for_pending_playback = false;
            runtime_state.save()?;
            Ok(true)
        }

        fn maybe_warn_hall_expiring(&mut self) -> Result<bool> {
            if !self.executor_is_idle()? {
                return Ok(false);
            }
            let minutes = {
                let runtime_state = self.runtime_state()?;
                let state = runtime_state.state();
                if state.hall_expiring_warning_sent {
                    return Ok(false);
                }
                let Some(minutes) = state.hall_remaining_minutes_now() else {
                    return Ok(false);
                };
                if minutes > HALL_EXPIRING_WARNING_MINUTES {
                    return Ok(false);
                }
                minutes
            };

            let message = if minutes == 0 {
                "大厅即将到期".to_string()
            } else {
                format!("大厅即将到期，剩余{}分钟", minutes)
            };
            self.reply(&message)?;

            let mut runtime_state = self.runtime_state()?;
            runtime_state.state_mut().hall_expiring_warning_sent = true;
            runtime_state.save()?;
            Ok(true)
        }

        fn resolve_song_request(
            &mut self,
            song: &command::SongCommand,
        ) -> Result<Option<ResolvedSongRequest>> {
            if !song.ai_assisted {
                return Ok(Some(ResolvedSongRequest {
                    keyword: song.keyword.clone(),
                    source: song.source.as_str().to_string(),
                    prefer_accompaniment: song.prefer_accompaniment,
                    ai_original_text: String::new(),
                    uri: String::new(),
                    skip_match_check: false,
                    friend_username: song.friend_username.clone(),
                }));
            }
            let label = song_label(song);
            if !self.ai.enabled() {
                self.reply(&format!("{}AI点歌未启用，请先配置 ai.api_key", label))?;
                return Ok(None);
            }

            self.reply(&format!("{}AI匹配中", label))?;

            let search_source = ai_candidate_source(song);
            let candidates = match self
                .feeluown
                .search_candidates(&song.keyword, search_source)
            {
                Ok(candidates) => candidates,
                Err(error) => {
                    log::error!("AI点歌搜索候选失败: {error:#}");
                    self.reply(&format!("{}平台无对应歌曲音源", label))?;
                    return Ok(None);
                }
            };
            if candidates.is_empty() {
                self.reply(&format!("{}平台无对应歌曲音源", label))?;
                return Ok(None);
            }

            let pick = match self.ai.pick_song_candidate(
                &song.keyword,
                song.prefer_accompaniment,
                &candidates,
            ) {
                Ok(pick) => pick,
                Err(error) => {
                    log::error!("AI点歌选择候选失败: {error:#}");
                    self.reply(&format!("{}AI点歌识别失败", label))?;
                    return Ok(None);
                }
            };
            let Some(candidate) = candidates
                .iter()
                .find(|candidate| candidate.uri == pick.uri)
            else {
                log::error!("AI点歌返回未知候选: {}", pick.uri);
                self.reply(&format!("{}AI点歌识别失败", label))?;
                return Ok(None);
            };
            log::info!(
                "AI点歌候选: raw={} pick={} uri={} score={:.2} reason={}",
                song.keyword,
                candidate.text,
                candidate.uri,
                pick.score,
                pick.reason
            );
            let message = format!("{}AI匹配:{},@确认@跳过", label, candidate.text);
            self.reply(&message)?;
            match self.wait_for_decision(false, false, true)? {
                UserDecision::Confirm | UserDecision::Timeout => {}
                UserDecision::Skip => return Ok(None),
                UserDecision::PromptFailed | UserDecision::Stopped => return Ok(None),
                _ => return Ok(None),
            }
            Ok(Some(ResolvedSongRequest {
                keyword: candidate.text.clone(),
                source: String::new(),
                prefer_accompaniment: song.prefer_accompaniment,
                ai_original_text: song.keyword.clone(),
                uri: candidate.uri.clone(),
                skip_match_check: true,
                friend_username: song.friend_username.clone(),
            }))
        }

        fn resolve_and_confirm_song(
            &mut self,
            song: &command::SongCommand,
        ) -> Result<Option<ResolvedSongRequest>> {
            let Some(request) = self.resolve_song_request(song)? else {
                return Ok(None);
            };
            if request.uri.is_empty() {
                let source = if request.source.trim().is_empty() {
                    "qqmusic"
                } else {
                    &request.source
                };
                let picked = match self.feeluown.search_and_pick(
                    &request.keyword,
                    source,
                    request.prefer_accompaniment,
                ) {
                    Ok(Some(picked)) => picked,
                    _ => {
                        let actions = if self.ai.enabled() {
                            "@换源@AI"
                        } else {
                            "@换源"
                        };
                        self.reply(&format!(
                            "{}平台无对应歌曲音源,{}",
                            request_label(&request),
                            actions
                        ))?;
                        let decision = self.wait_for_decision(true, self.ai.enabled(), true)?;
                        match decision {
                            UserDecision::SwitchSource => {
                                let next_source = if source == "netease" {
                                    "qqmusic"
                                } else {
                                    "netease"
                                };
                                return self
                                    .resolve_and_confirm_song_with_source(song, next_source);
                            }
                            UserDecision::Ai if self.ai.enabled() => {
                                return self.resolve_and_confirm_song_ai(song);
                            }
                            _ => return Ok(None),
                        }
                    }
                };
                let song_title = picked.0.text.clone();
                let uri = picked.0.uri.clone();
                let actions = if self.ai.enabled() {
                    "@确认@跳过@换源@AI"
                } else {
                    "@确认@跳过@换源"
                };
                self.reply(&format!(
                    "{}搜索到:{},{}",
                    request_label(&request),
                    song_title,
                    actions
                ))?;
                let decision = self.wait_for_decision(true, self.ai.enabled(), true)?;
                match decision {
                    UserDecision::Confirm | UserDecision::Timeout => {
                        return Ok(Some(ResolvedSongRequest {
                            keyword: picked.0.text.clone(),
                            source: source.to_string(),
                            prefer_accompaniment: request.prefer_accompaniment,
                            ai_original_text: String::new(),
                            uri,
                            skip_match_check: false,
                            friend_username: request.friend_username.clone(),
                        }));
                    }
                    UserDecision::Skip => {
                        return Ok(None);
                    }
                    UserDecision::SwitchSource => {
                        let next_source = if source == "netease" {
                            "qqmusic"
                        } else {
                            "netease"
                        };
                        return self.resolve_and_confirm_song_with_source(song, next_source);
                    }
                    UserDecision::Ai if self.ai.enabled() => {
                        return self.resolve_and_confirm_song_ai(song);
                    }
                    _ => return Ok(None),
                }
            }
            Ok(Some(request))
        }

        fn resolve_and_confirm_song_with_source(
            &mut self,
            song: &command::SongCommand,
            source: &str,
        ) -> Result<Option<ResolvedSongRequest>> {
            let picked = match self.feeluown.search_and_pick(
                &song.keyword,
                source,
                song.prefer_accompaniment,
            ) {
                Ok(Some(picked)) => picked,
                _ => {
                    let actions = if self.ai.enabled() {
                        "@换源@AI"
                    } else {
                        "@换源"
                    };
                    self.reply(&format!("{}换源后仍无音源,{}", song_label(song), actions))?;
                    let decision = self.wait_for_decision(true, self.ai.enabled(), true)?;
                    match decision {
                        UserDecision::SwitchSource => {
                            let next_source = if source == "netease" {
                                "qqmusic"
                            } else {
                                "netease"
                            };
                            return self.resolve_and_confirm_song_with_source(song, next_source);
                        }
                        UserDecision::Ai if self.ai.enabled() => {
                            return self.resolve_and_confirm_song_ai(song);
                        }
                        _ => return Ok(None),
                    }
                }
            };
            let actions = if self.ai.enabled() {
                "@确认@跳过@换源@AI"
            } else {
                "@确认@跳过@换源"
            };
            self.reply(&format!(
                "{}搜索到:{},{}",
                song_label(song),
                picked.0.text,
                actions
            ))?;
            let decision = self.wait_for_decision(true, self.ai.enabled(), true)?;
            match decision {
                UserDecision::Confirm | UserDecision::Timeout => Ok(Some(ResolvedSongRequest {
                    keyword: picked.0.text.clone(),
                    source: source.to_string(),
                    prefer_accompaniment: song.prefer_accompaniment,
                    ai_original_text: String::new(),
                    uri: picked.0.uri.clone(),
                    skip_match_check: false,
                    friend_username: song.friend_username.clone(),
                })),
                UserDecision::Skip => Ok(None),
                UserDecision::SwitchSource => {
                    let next_source = if source == "netease" {
                        "qqmusic"
                    } else {
                        "netease"
                    };
                    self.resolve_and_confirm_song_with_source(song, next_source)
                }
                UserDecision::Ai if self.ai.enabled() => self.resolve_and_confirm_song_ai(song),
                _ => Ok(None),
            }
        }

        fn resolve_and_confirm_song_ai(
            &mut self,
            song: &command::SongCommand,
        ) -> Result<Option<ResolvedSongRequest>> {
            let label = song_label(song);
            if !self.ai.enabled() {
                self.reply(&format!("{}AI点歌未启用", label))?;
                return Ok(None);
            }
            self.reply(&format!("{}AI匹配中", label))?;
            let search_source = ai_candidate_source(song);
            let candidates = match self
                .feeluown
                .search_candidates(&song.keyword, search_source)
            {
                Ok(candidates) => candidates,
                Err(error) => {
                    log::error!("AI点歌搜索候选失败: {error:#}");
                    self.reply(&format!("{}平台无对应歌曲音源", label))?;
                    return Ok(None);
                }
            };
            if candidates.is_empty() {
                self.reply(&format!("{}平台无对应歌曲音源", label))?;
                return Ok(None);
            }
            let pick = match self.ai.pick_song_candidate(
                &song.keyword,
                song.prefer_accompaniment,
                &candidates,
            ) {
                Ok(pick) => pick,
                Err(error) => {
                    log::error!("AI点歌选择候选失败: {error:#}");
                    self.reply(&format!("{}AI点歌识别失败", label))?;
                    return Ok(None);
                }
            };
            let Some(candidate) = candidates.iter().find(|c| c.uri == pick.uri) else {
                log::error!("AI点歌返回未知候选: {}", pick.uri);
                self.reply(&format!("{}AI点歌识别失败", label))?;
                return Ok(None);
            };
            log::info!(
                "AI点歌候选: raw={} pick={} uri={} score={:.2} reason={}",
                song.keyword,
                candidate.text,
                candidate.uri,
                pick.score,
                pick.reason
            );
            let message = format!("{}AI匹配:{},@确认@跳过", label, candidate.text);
            self.reply(&message)?;
            match self.wait_for_decision(false, false, true)? {
                UserDecision::Confirm | UserDecision::Timeout => {}
                UserDecision::Skip => return Ok(None),
                UserDecision::PromptFailed | UserDecision::Stopped => return Ok(None),
                _ => return Ok(None),
            }
            Ok(Some(ResolvedSongRequest {
                keyword: candidate.text.clone(),
                source: String::new(),
                prefer_accompaniment: song.prefer_accompaniment,
                ai_original_text: song.keyword.clone(),
                uri: candidate.uri.clone(),
                skip_match_check: true,
                friend_username: song.friend_username.clone(),
            }))
        }

        fn queue_contains_request(&self, request: &ResolvedSongRequest) -> Result<bool> {
            let queue = self.queue()?;
            if !request.uri.is_empty() {
                return Ok(queue.has_duplicate_uri(&request.uri));
            }
            Ok(queue.has_duplicate(
                &request.keyword,
                &request.source,
                request.prefer_accompaniment,
            ))
        }

        fn push_queue_request(&self, request: &ResolvedSongRequest) -> Result<Option<usize>> {
            let mut queue = self.queue()?;
            if queue.is_full() {
                return Ok(None);
            }
            queue.push(queue::QueueItem {
                keyword: request.keyword.clone(),
                source: request.source.clone(),
                prefer_accompaniment: request.prefer_accompaniment,
                ai_original_text: request.ai_original_text.clone(),
                uri: request.uri.clone(),
                friend_username: request.friend_username.clone(),
            })?;
            Ok(Some(queue.len()))
        }

        fn execute_command(&mut self, parsed: &ParsedCommand) -> Result<()> {
            match &parsed.command {
                UserCommand::Song(song) => {
                    let Some(request) = self.resolve_and_confirm_song(song)? else {
                        return Ok(());
                    };
                    if self.queue_contains_request(&request)? {
                        log::info!("队列已有: {}", request.keyword);
                        self.log_executed_command(
                            parsed,
                            &final_song_command_text(&request, "duplicate"),
                        )?;
                        self.reply(&format!("队列已有: {}", request.keyword))?;
                        return Ok(());
                    }
                    if !self.queue()?.is_empty() {
                        let added_len = self.push_queue_request(&request)?;
                        if let Some(len) = added_len {
                            self.log_executed_command(
                                parsed,
                                &final_song_command_text(&request, "queue"),
                            )?;
                            self.reply(&format!(
                                "队列已加入({}/{}): {}",
                                len, self.config.queue.max_size, request.keyword
                            ))?;
                        } else {
                            self.log_executed_command(
                                parsed,
                                &final_song_command_text(&request, "queue-full"),
                            )?;
                            self.reply("队列已满，请稍后再试")?;
                        }
                        return Ok(());
                    }

                    let status = self.feeluown.status();
                    match status {
                        Ok(status) if is_playing(&status) => {
                            if !request.skip_match_check {
                                let current_match = song_matcher::match_song_query(
                                    &self.config.matching,
                                    request.match_keyword(),
                                    &status.name,
                                    &status.singer,
                                    request.prefer_accompaniment,
                                );
                                if current_match.ok {
                                    self.log_executed_command(
                                        parsed,
                                        &final_song_command_text(&request, "already-playing"),
                                    )?;
                                    self.reply(&format!("当前正在播放: {}", request.keyword))?;
                                    return Ok(());
                                }
                            }
                            if !self.runtime_state()?.state().current_song_is_requested {
                                let mut runtime_state = self.runtime_state()?;
                                runtime_state.state_mut().paused_by_command = false;
                                runtime_state.save()?;
                                drop(runtime_state);
                                self.log_executed_command(
                                    parsed,
                                    &final_song_command_text(&request, "play"),
                                )?;
                                let _ = self.play_request_confirmed(&request, true)?;
                                return Ok(());
                            }
                            let added_len = self.push_queue_request(&request)?;
                            if let Some(len) = added_len {
                                self.log_executed_command(
                                    parsed,
                                    &final_song_command_text(&request, "queue"),
                                )?;
                                self.reply(&format!(
                                    "队列已加入({}/{}): {}",
                                    len, self.config.queue.max_size, request.keyword
                                ))?;
                            } else {
                                self.log_executed_command(
                                    parsed,
                                    &final_song_command_text(&request, "queue-full"),
                                )?;
                                self.reply("队列已满，请稍后再试")?;
                            }
                            return Ok(());
                        }
                        Ok(_) => {
                            let mut runtime_state = self.runtime_state()?;
                            runtime_state.state_mut().paused_by_command = false;
                            runtime_state.save()?;
                        }
                        Err(error) => {
                            log::error!("获取播放状态失败: {error:#}");
                            let added_len = self.push_queue_request(&request)?;
                            if let Some(len) = added_len {
                                self.log_executed_command(
                                    parsed,
                                    &final_song_command_text(&request, "queue-status-unknown"),
                                )?;
                                self.reply(&format!(
                                    "状态未知，队列已加入({}/{}): {}",
                                    len, self.config.queue.max_size, request.keyword
                                ))?;
                            } else {
                                self.log_executed_command(
                                    parsed,
                                    &final_song_command_text(&request, "queue-full-status-unknown"),
                                )?;
                                self.reply("状态未知且队列已满，请稍后再试")?;
                            }
                            return Ok(());
                        }
                    }

                    self.log_executed_command(parsed, &final_song_command_text(&request, "play"))?;
                    let _ = self.play_request_confirmed(&request, true)?;
                }
                UserCommand::Pause => {
                    let message = self.feeluown.pause()?;
                    self.log_executed_command(parsed, "pause")?;
                    let mut runtime_state = self.runtime_state()?;
                    runtime_state.state_mut().paused_by_command = true;
                    runtime_state.state_mut().paused_for_pending_playback = false;
                    runtime_state.save()?;
                    self.reply(if message.trim().is_empty() {
                        "已暂停"
                    } else {
                        message.trim()
                    })?;
                }
                UserCommand::Resume | UserCommand::Play => {
                    let message = self.feeluown.play()?;
                    self.log_executed_command(parsed, "resume")?;
                    let mut runtime_state = self.runtime_state()?;
                    runtime_state.state_mut().paused_by_command = false;
                    runtime_state.state_mut().paused_for_pending_playback = false;
                    runtime_state.save()?;
                    self.reply(if message.trim().is_empty() {
                        "已恢复播放"
                    } else {
                        message.trim()
                    })?;
                }
                UserCommand::Next => {
                    if !self.queue()?.is_empty() {
                        self.consume_queue("手动下一首")?;
                        self.log_executed_command(parsed, "next queue")?;
                    } else {
                        let message = self.feeluown.next()?;
                        self.log_executed_command(parsed, "next feeluown")?;
                        self.reply_player_status_after_skip(message.trim())?;
                    }
                }
                UserCommand::Previous => {
                    let message = self.feeluown.previous()?;
                    self.log_executed_command(parsed, "previous")?;
                    self.reply_player_status_after_skip(message.trim())?;
                }
                UserCommand::Volume(volume) => {
                    self.feeluown.set_volume(volume)?;
                    self.log_executed_command(parsed, &format!("volume {}", volume))?;
                    self.reply(&format!("音量已设置为 {}", volume))?;
                }
                UserCommand::Status => {
                    let status = self.feeluown.status()?;
                    self.log_executed_command(parsed, "status")?;
                    self.reply(&format_status(&status))?;
                }
                UserCommand::Lyrics => {
                    let status = self.feeluown.status()?;
                    self.log_executed_command(parsed, "lyrics")?;
                    self.reply(&format_lyrics(&status))?;
                }
                UserCommand::Queue => {
                    self.log_executed_command(parsed, "queue list")?;
                    self.log_queue()?;
                }
                UserCommand::QueueDelete(indexes) => {
                    if indexes.is_empty() {
                        self.log_executed_command(parsed, "queue delete invalid")?;
                        self.reply("没有匹配到有效队列序号")?;
                        return Ok(());
                    }
                    let removed = self.queue()?.remove_indexes(indexes)?;
                    if removed.is_empty() {
                        self.log_executed_command(parsed, "queue delete none")?;
                        self.reply("队列删除失败或序号不存在")?;
                    } else {
                        let removed_text = removed
                            .iter()
                            .map(|(index, item)| format!("{}.{}", index, item.keyword))
                            .collect::<Vec<_>>()
                            .join(", ");
                        self.log_executed_command(
                            parsed,
                            &format!("queue delete {}", removed_text),
                        )?;
                        self.reply(&format!("队列已删除: {}", removed_text))?;
                    }
                }
                UserCommand::QueueClear => {
                    let count = self.queue()?.clear()?;
                    self.log_executed_command(parsed, &format!("queue clear {}", count))?;
                    if count == 0 {
                        self.reply("队列为空")?;
                    } else {
                        self.reply(&format!("队列已清空: {} 首", count))?;
                    }
                }
                UserCommand::HallDetect => {
                    self.log_executed_command(parsed, "hall detect")?;
                    self.execute_hall_detect()?;
                }
                UserCommand::HallTime => {
                    self.log_executed_command(parsed, "hall time")?;
                    self.reply_hall_time()?;
                }
                UserCommand::Help => {
                    self.log_executed_command(parsed, "help")?;
                    self.send_help()?;
                }
                UserCommand::Invite(invite) => {
                    {
                        let mut executed = self
                            .invite_executed_seqs
                            .lock()
                            .map_err(|_| anyhow!("invite_executed_seqs mutex poisoned"))?;
                        if !executed.insert(invite.seq) {
                            log::info!("邀请参数 {} 已执行过，跳过", invite.seq);
                            return Ok(());
                        }
                    }
                    self.log_executed_command(parsed, &format!("invite {}", invite.username))?;
                    self.execute_invite_with_announce(&invite.username)?;
                }
                UserCommand::Microphone { username } => {
                    log::info!("收到麦克风命令: {}", username);
                    if self.check_public_hall()? {
                        self.log_executed_command(
                            parsed,
                            &format!("microphone skipped publicHall {}", username),
                        )?;
                        log::info!("麦克风: 当前在公共大厅，跳过状态切换和通告");
                    } else {
                        self.log_executed_command(
                            parsed,
                            &format!("microphone toggle {}", username),
                        )?;
                        self.execute_microphone_command(username)?;
                    }
                }
                UserCommand::DisableCommands { username: _ } => {
                    log::info!("收到禁用命令");
                    self.commands_enabled.store(false, AtomicOrdering::SeqCst);
                    self.log_executed_command(parsed, "disable commands")?;
                    self.reply("管理员已禁用大厅命令识别功能")?;
                }
                UserCommand::EnableCommands { username: _ } => {
                    log::info!("收到启用命令");
                    self.commands_enabled.store(true, AtomicOrdering::SeqCst);
                    self.log_executed_command(parsed, "enable commands")?;
                    self.reply("管理员已启用大厅命令识别功能")?;
                }
                UserCommand::IdleExit { minutes } => {
                    self.log_executed_command(parsed, &format!("idle exit {}", minutes))?;
                }
            };
            Ok(())
        }

        fn configure_idle_exit(&self, minutes: u32) -> Result<()> {
            let minutes = minutes.max(IDLE_EXIT_MIN_MINUTES);
            let mut state = self
                .idle_exit
                .lock()
                .map_err(|_| anyhow!("idle_exit mutex poisoned"))?;
            *state = Some(IdleExitState {
                timeout: Duration::from_secs(minutes as u64 * 60),
                last_command_at: Instant::now(),
            });
            log::info!(
                "已设置闲置退出: {}分钟无新命令后关闭目标窗口并退出",
                minutes
            );
            Ok(())
        }

        fn execute_microphone_command(&self, username: &str) -> Result<()> {
            if !self.is_primary_ui()? {
                log::info!("麦克风: 当前不在一级界面，返回一级界面");
                self.return_to_primary_fixed();
            }
            log::info!("麦克风: 按 N 切换状态");
            press_key(Key::Unicode('n'), &self.config.window)?;
            sleep(Duration::from_millis(100));
            self.reply(&format!("@{} 执行了切换麦克风状态！", username))
        }

        fn is_primary_ui(&self) -> Result<bool> {
            let templates = UiTemplateArgs::default().resolve(&self.config);
            let canvas = Canvas {
                width: self.config.screen.expected_width,
                height: self.config.screen.expected_height,
                resize: true,
            };
            let frame = load_frame(&FrameArgs { image: None }, &canvas, &self.config.window)?;
            let ui_state = detect_ui_state(&frame.image, &templates, &self.config.screen)?;
            Ok(ui_state.is_primary())
        }

        fn execute_hall_detect(&mut self) -> Result<()> {
            log::info!("大厅检测: 按 F2 进入大厅页面");
            press_key(Key::F2, &self.config.window)?;
            sleep(Duration::from_millis(
                self.config.timing.hall_page_settle_ms,
            ));

            let result = self.read_hall_info();

            self.return_to_primary_from_transient_ui("大厅检测");

            match result {
                Ok(info) => {
                    let name = info.name;
                    log::info!("大厅检测 OCR 结果: {}", name);
                    if command::normalize_lock_text(&name)
                        == command::normalize_lock_text("公共大厅")
                    {
                        self.clear_hall_remaining_minutes()?;
                        self.reply("当前为公共大厅")?;
                    } else {
                        if let Some(minutes) = info.remaining_minutes {
                            self.update_hall_remaining_minutes(minutes)?;
                            log::info!("大厅剩余时间 OCR 结果: {}分钟", minutes);
                        }
                        let time_suffix = format_hall_remaining_suffix(info.remaining_minutes);
                        self.reply(&format!(
                            "当前为{}{}",
                            if name.is_empty() {
                                "未识别到大厅名称"
                            } else {
                                name.as_str()
                            },
                            time_suffix
                        ))?;
                    }
                }
                Err(error) => {
                    log::error!("大厅检测 OCR 失败: {error:#}");
                    self.reply("大厅检测失败")?;
                }
            }
            Ok(())
        }

        fn reply_player_status_after_skip(&self, fallback: &str) -> Result<()> {
            sleep(Duration::from_millis(
                self.config.timing.skip_status_initial_ms,
            ));
            for _ in 0..self.config.timing.skip_status_retries {
                match self.feeluown.status() {
                    Ok(status) if is_playing(&status) || status.status == "paused" => {
                        return self.reply(&format_play_message(&status));
                    }
                    Ok(_) => sleep(Duration::from_millis(
                        self.config.timing.skip_status_poll_ms,
                    )),
                    Err(error) => {
                        log::error!("切歌后查询播放状态失败: {error:#}");
                        break;
                    }
                }
            }
            if fallback.is_empty() {
                self.reply("切歌完成")
            } else {
                self.reply(fallback)
            }
        }

        fn reply_hall_time(&mut self) -> Result<()> {
            let minutes = self.runtime_state()?.state().hall_remaining_minutes_now();
            if let Some(minutes) = minutes.filter(|minutes| *minutes > 0) {
                return self.reply(&format!("大厅到期时间，剩余{}分钟", minutes));
            }

            log::info!("大厅时间未知，执行一次大厅识别");
            press_key(Key::F2, &self.config.window)?;
            sleep(Duration::from_millis(
                self.config.timing.hall_page_settle_ms,
            ));
            let result = self.read_hall_info();
            self.return_to_primary_from_transient_ui("大厅时间");

            let info = match result {
                Ok(info) => info,
                Err(error) => {
                    log::error!("大厅时间 OCR 失败: {error:#}");
                    return self.reply("大厅时间未知");
                }
            };
            let is_public_hall = command::normalize_lock_text(&info.name)
                == command::normalize_lock_text("公共大厅");
            if is_public_hall {
                self.clear_hall_remaining_minutes()?;
                return self.reply("公共大厅无时间限制");
            }
            if let Some(minutes) = info.remaining_minutes {
                self.update_hall_remaining_minutes(minutes)?;
                return self.reply(&format!("大厅到期时间，剩余{}分钟", minutes));
            }
            self.reply("大厅时间未知")
        }

        fn check_public_hall(&self) -> Result<bool> {
            log::info!("大厅检测: 按 F2 进入大厅页面");
            press_key(Key::F2, &self.config.window)?;
            sleep(Duration::from_millis(
                self.config.timing.hall_page_settle_ms,
            ));
            let result = self.read_hall_info();
            self.return_to_primary_from_transient_ui("大厅检测");
            let info = match result {
                Ok(info) => info,
                Err(error) => {
                    log::error!("大厅检测 OCR 失败，按非公共大厅处理: {error:#}");
                    return Ok(false);
                }
            };
            let name = info.name;
            log::info!("大厅检测 OCR 结果: {}", name);
            let is_public_hall =
                command::normalize_lock_text(&name) == command::normalize_lock_text("公共大厅");
            if is_public_hall {
                self.clear_hall_remaining_minutes()?;
            } else if let Some(minutes) = info.remaining_minutes {
                self.update_hall_remaining_minutes(minutes)?;
                log::info!("大厅剩余时间 OCR 结果: {}分钟", minutes);
            }
            Ok(is_public_hall)
        }

        fn execute_invite_with_announce(&mut self, username: &str) -> Result<bool> {
            log::info!("邀请: 先检测是否公共大厅");
            if self.check_public_hall()? {
                log::info!("邀请: 当前在公共大厅，直接执行");
                self.notify_friend_invite_decision(username, "已同意加入大厅,请注意启动麦克风");
                return self.execute_invite(username);
            }
            let announce = format!(
                "{}邀请BOT前往大厅,30s内@邀请确认@邀请拒绝,默认通过",
                username
            );
            if let Err(error) = self.reply(&announce) {
                log::error!("邀请通告发送失败，直接执行邀请: {error:#}");
                return self.execute_invite(username);
            }
            match self.wait_for_invite_decision()? {
                Some(true) => {
                    self.notify_friend_invite_decision(username, "已同意加入大厅,请注意启动麦克风");
                    self.execute_invite(username)
                }
                None => {
                    self.notify_friend_invite_decision(
                        username,
                        "已默认同意加入大厅,请注意启动麦克风",
                    );
                    self.execute_invite(username)
                }
                Some(false) => {
                    log::info!("收到邀请拒绝，取消邀请");
                    self.notify_friend_invite_decision(username, "大厅成员已拒绝邀请");
                    self.return_to_primary_fixed();
                    Ok(false)
                }
            }
        }

        fn on_entered_new_hall(&self) {
            log::info!("已进入新大厅，重置命令识别状态");
            self.commands_enabled.store(true, AtomicOrdering::SeqCst);
            self.screen_lock_primed.store(false, AtomicOrdering::SeqCst);
            self.reset_locks_requested
                .store(true, AtomicOrdering::SeqCst);
        }

        fn notify_friend_invite_decision(&self, username: &str, message: &str) {
            if let Err(error) = self.send_friend_message(username, message) {
                log::error!("好友邀请确认回复失败: {error:#}");
            }
        }

        fn wait_for_invite_decision(&self) -> Result<Option<bool>> {
            let existing = self.collect_invite_decision_bottoms();
            let deadline = Instant::now()
                + Duration::from_millis(self.config.timing.invite_confirm_timeout_ms);
            let template_args = TemplateArgs::default().resolve(&self.config);
            let canvas = Canvas {
                width: self.config.screen.expected_width,
                height: self.config.screen.expected_height,
                resize: true,
            };
            while self.running.load(AtomicOrdering::SeqCst) && Instant::now() < deadline {
                sleep(Duration::from_millis(
                    self.config.timing.invite_confirm_poll_ms,
                ));
                let frame =
                    match load_frame(&FrameArgs { image: None }, &canvas, &self.config.window) {
                        Ok(frame) => frame,
                        Err(error) => {
                            log::error!("邀请确认截图失败: {error:#}");
                            continue;
                        }
                    };
                let scan_result = {
                    let engine = match self.ocr_engine() {
                        Ok(engine) => engine,
                        Err(error) => {
                            log::error!("邀请确认 OCR 锁失败: {error:#}");
                            continue;
                        }
                    };
                    scan_chat(
                        &frame.image,
                        &engine.engine,
                        &template_args,
                        self.config.screen.chat_rect.into(),
                    )
                };
                let messages = match scan_result {
                    Ok(messages) => messages,
                    Err(error) => {
                        log::error!("邀请确认扫描失败: {error:#}");
                        continue;
                    }
                };
                for message in messages {
                    if message.message_type != "blue" {
                        continue;
                    }
                    if is_existing_decision(&message, &existing) {
                        continue;
                    }
                    match parse_invite_decision(&message.text) {
                        Some(true) => return Ok(Some(true)),
                        Some(false) => return Ok(Some(false)),
                        None => {}
                    }
                }
            }
            Ok(None)
        }

        fn collect_invite_decision_bottoms(&self) -> HashMap<String, i32> {
            let mut output = HashMap::new();
            let template_args = TemplateArgs::default().resolve(&self.config);
            let canvas = Canvas {
                width: self.config.screen.expected_width,
                height: self.config.screen.expected_height,
                resize: true,
            };
            let Ok(frame) = load_frame(&FrameArgs { image: None }, &canvas, &self.config.window)
            else {
                return output;
            };
            let Ok(engine) = self.ocr_engine() else {
                return output;
            };
            let Ok(messages) = scan_chat(
                &frame.image,
                &engine.engine,
                &template_args,
                self.config.screen.chat_rect.into(),
            ) else {
                return output;
            };
            for message in messages {
                if message.message_type != "blue" {
                    continue;
                }
                if parse_invite_decision(&message.text).is_some() {
                    let bottom = message.block.y + message.block.height as i32;
                    output
                        .entry(message.text)
                        .and_modify(|value| *value = (*value).max(bottom))
                        .or_insert(bottom);
                }
            }
            output
        }

        fn execute_invite(&self, username: &str) -> Result<bool> {
            log::info!("开始邀请: {}", username);
            let result = self.execute_invite_steps(username);
            if result.is_err() {
                self.return_to_primary_from_transient_ui("邀请失败");
            } else if matches!(result, Ok(true)) {
                log::info!("邀请成功，等待 10s 后兜底返回一级界面");
                sleep(Duration::from_secs(10));
                self.return_to_primary_fixed();
            }
            result
        }

        fn execute_invite_steps(&self, username: &str) -> Result<bool> {
            let canvas = Canvas {
                width: self.config.screen.expected_width,
                height: self.config.screen.expected_height,
                resize: true,
            };
            if !self.open_friend_chat(username, &canvas)? {
                return Ok(false);
            }
            let frame_args = FrameArgs { image: None };

            let point = {
                let engine = self.ocr_engine()?;
                self.find_text_point(
                    &engine.engine,
                    &canvas,
                    self.config.invite.confirm_list_region.into(),
                    username,
                )?
            };
            let Some(point) = point else {
                log::error!("邀请失败: 确认列表未找到用户 {}", username);
                self.return_to_primary_from_transient_ui("邀请失败");
                return Ok(false);
            };
            click_game_point(PointConfig::new(point.x, point.y), &self.config.window)?;
            sleep(Duration::from_millis(self.config.timing.invite_step_ms));

            for (label, rect, template) in [
                (
                    "查看千星",
                    self.config.invite.view_star_region.into(),
                    self.config.templates.invite_view_star.clone(),
                ),
                (
                    "前往其大厅",
                    self.config.invite.goto_hall_region.into(),
                    self.config.templates.invite_goto_hall.clone(),
                ),
                (
                    "进入大厅",
                    self.config.invite.enter_hall_region.into(),
                    self.config.templates.invite_enter_hall.clone(),
                ),
            ] {
                let frame = load_frame(&frame_args, &canvas, &self.config.window)?;
                let Some(hit) = best_template_hit(
                    &frame.image,
                    Some(rect),
                    &template,
                    self.config.templates.marker_threshold,
                )?
                else {
                    log::error!("邀请失败: 未找到{}按钮", label);
                    self.return_to_primary_from_transient_ui("邀请失败");
                    return Ok(false);
                };
                let center = hit.center();
                click_game_point(PointConfig::new(center.x, center.y), &self.config.window)?;
                if label == "进入大厅" {
                    self.on_entered_new_hall();
                }
                sleep(Duration::from_millis(self.config.timing.invite_step_ms));
            }

            log::info!("邀请完成: {}", username);
            Ok(true)
        }

        fn send_friend_message(&self, username: &str, message: &str) -> Result<bool> {
            log::info!("好友发言: {} -> {}", username, message);
            let canvas = Canvas {
                width: self.config.screen.expected_width,
                height: self.config.screen.expected_height,
                resize: true,
            };
            let opened = match self.open_friend_chat(username, &canvas) {
                Ok(opened) => opened,
                Err(error) => {
                    self.return_to_primary_from_transient_ui("好友发言失败");
                    return Err(error);
                }
            };
            if !opened {
                return Ok(false);
            }
            let result = self.chat_output.send_current_chat(message);
            self.return_to_primary_from_transient_ui("好友发言");
            result?;
            Ok(true)
        }

        fn open_friend_chat(&self, username: &str, canvas: &Canvas) -> Result<bool> {
            click_game_point(self.config.output.focus_point, &self.config.window)?;
            sleep(Duration::from_millis(
                self.config.timing.invite_open_chat_ms,
            ));
            press_key(Key::Return, &self.config.window)?;
            sleep(Duration::from_millis(
                self.config.timing.invite_open_chat_ms,
            ));

            let point = {
                let engine = self.ocr_engine()?;
                self.find_text_point(
                    &engine.engine,
                    canvas,
                    self.config.invite.friend_list_region.into(),
                    username,
                )?
            };
            let Some(point) = point else {
                log::error!("好友聊天失败: 好友列表未找到用户 {}", username);
                self.return_to_primary_from_transient_ui("好友聊天失败");
                return Ok(false);
            };
            click_game_point(PointConfig::new(point.x, point.y), &self.config.window)?;
            sleep(Duration::from_millis(self.config.timing.invite_step_ms));
            Ok(true)
        }

        fn find_text_point(
            &self,
            engine: &OcrEngine,
            canvas: &Canvas,
            rect: Rect,
            expected: &str,
        ) -> Result<Option<Point>> {
            let frame = load_frame(&FrameArgs { image: None }, canvas, &self.config.window)?;
            let crop = crop_canvas(&frame.image, rect)?;
            let target = command::normalize_lock_text(expected);
            if target.is_empty() {
                return Ok(None);
            }
            let mut fallback = None;
            for line in recognize_lines(engine, &crop)? {
                let norm = command::normalize_lock_text(&line.text);
                if norm.is_empty() {
                    continue;
                }
                let point = Point::new(
                    rect.x + line.bbox.x + line.bbox.width as i32 / 2,
                    rect.y + line.bbox.y + line.bbox.height as i32 / 2,
                );
                if norm == target {
                    return Ok(Some(point));
                }
                if fallback.is_none() && (norm.contains(&target) || target.contains(&norm)) {
                    fallback = Some(point);
                }
            }
            Ok(fallback)
        }

        fn return_to_primary_fixed(&self) {
            let templates = UiTemplateArgs::default().resolve(&self.config);
            let canvas = Canvas {
                width: self.config.screen.expected_width,
                height: self.config.screen.expected_height,
                resize: true,
            };
            let frame_args = FrameArgs { image: None };
            let deadline =
                Instant::now() + Duration::from_millis(self.config.timing.command_ui_timeout_ms);

            while Instant::now() < deadline {
                match load_frame(&frame_args, &canvas, &self.config.window).and_then(|frame| {
                    detect_ui_state(&frame.image, &templates, &self.config.screen)
                }) {
                    Ok(ui_state) if ui_state.is_primary() => {
                        log::info!("已返回一级界面: {}", ui_state);
                        return;
                    }
                    Ok(ui_state) => {
                        log::info!("返回一级界面中，当前: {}，按 ESC", ui_state);
                    }
                    Err(error) => {
                        log::error!("返回一级界面检测失败，继续按 ESC: {error:#}");
                    }
                }
                if let Err(error) = press_key(Key::Escape, &self.config.window) {
                    log::error!("返回一级界面按 ESC 失败: {error:#}");
                    return;
                }
                sleep(Duration::from_millis(
                    self.config.timing.return_to_primary_retry_ms,
                ));
            }
            log::error!("返回一级界面超时");
        }

        fn return_to_primary_after_command_failure(&self, command: &str) {
            log::info!("命令失败后返回一级界面: {}", command);
            self.return_to_primary_fixed();
        }

        fn return_to_primary_from_transient_ui(&self, context: &str) {
            log::info!("{}: 先按 ESC 关闭临时界面", context);
            if let Err(error) = press_key(Key::Escape, &self.config.window) {
                log::error!("{}: 关闭临时界面失败: {error:#}", context);
            } else {
                sleep(Duration::from_millis(
                    self.config.timing.return_to_primary_retry_ms,
                ));
            }
            self.return_to_primary_fixed();
        }

        fn read_hall_info(&self) -> Result<HallInfo> {
            let canvas = Canvas {
                width: self.config.screen.expected_width,
                height: self.config.screen.expected_height,
                resize: true,
            };

            let mut samples = Vec::new();
            for index in 0..HALL_INFO_OCR_SAMPLES {
                if index > 0 {
                    sleep(Duration::from_millis(
                        self.config.timing.hall_ocr_sample_interval_ms,
                    ));
                }
                let frame = load_frame(&FrameArgs { image: None }, &canvas, &self.config.window)?;
                let sample = self.read_hall_info_sample_from_frame(&frame.image)?;
                log::info!(
                    "大厅检测 OCR 采样: {}/{} name={} time={} minutes={}",
                    index + 1,
                    HALL_INFO_OCR_SAMPLES,
                    display_or_empty(&sample.name),
                    display_or_empty(&sample.time_text),
                    sample
                        .remaining_minutes
                        .map(|minutes| minutes.to_string())
                        .unwrap_or_else(|| "未知".to_string())
                );
                samples.push(sample);
            }
            Ok(merge_hall_info_samples(&samples))
        }

        fn read_hall_info_sample_from_frame(&self, image: &DynamicImage) -> Result<HallInfoSample> {
            let name_crop = crop_canvas(image, self.config.screen.hall_name_rect.into())?;
            let name = {
                let engine = self.ocr_engine()?;
                merged_ocr_text(
                    &engine.engine,
                    &name_crop,
                    self.config.ocr.same_line_y_tolerance,
                )?
            };
            let time_crop = crop_canvas(image, self.config.screen.hall_time_rect.into())?;
            let time_text = {
                let engine = self.ocr_engine()?;
                merged_ocr_text(
                    &engine.engine,
                    &time_crop,
                    self.config.ocr.same_line_y_tolerance,
                )?
            };
            Ok(HallInfoSample {
                name,
                time_text: time_text.clone(),
                remaining_minutes: parse_hall_remaining_minutes(&time_text),
            })
        }

        fn update_hall_remaining_minutes(&self, minutes: u32) -> Result<()> {
            let mut runtime_state = self.runtime_state()?;
            runtime_state
                .state_mut()
                .update_hall_remaining_minutes(minutes);
            runtime_state.save()
        }

        fn clear_hall_remaining_minutes(&self) -> Result<()> {
            let mut runtime_state = self.runtime_state()?;
            runtime_state.state_mut().clear_hall_remaining_minutes();
            runtime_state.save()
        }

        fn send_help(&self) -> Result<()> {
            self.chat_output.send_batch(
                &[
                    "点歌示例: @点歌/@AI点歌 歌名 歌手 伴奏,输入伴奏时优先匹配伴奏",
                "命令以@开头: 暂停、继续、播放、下一首、上一首、状态、歌词、帮助、队列、音量1-100",
                    "切换网易平台: @网易点歌 歌名 歌手 伴奏,默认为QQ平台",
                ],
                self.config.timing.help_batch_ms,
            )
        }

        fn play_keyword_confirmed(
            &mut self,
            keyword: &str,
            source: &str,
            prefer_accompaniment: bool,
            allow_switch_source: bool,
        ) -> Result<PlayOutcome> {
            self.play_keyword_confirmed_inner(
                keyword,
                source,
                prefer_accompaniment,
                allow_switch_source,
                false,
            )
        }

        fn play_request_confirmed(
            &mut self,
            request: &ResolvedSongRequest,
            allow_switch_source: bool,
        ) -> Result<PlayOutcome> {
            if request.uri.trim().is_empty() {
                return self.play_keyword_confirmed(
                    &request.keyword,
                    &request.source,
                    request.prefer_accompaniment,
                    allow_switch_source,
                );
            }
            self.play_uri_confirmed(
                &request.uri,
                &request.keyword,
                request.match_keyword(),
                request.prefer_accompaniment,
                request.skip_match_check,
            )
        }

        fn play_uri_confirmed(
            &mut self,
            uri: &str,
            _display_keyword: &str,
            match_keyword: &str,
            prefer_accompaniment: bool,
            skip_match_check: bool,
        ) -> Result<PlayOutcome> {
            self.clear_requested_song_state()?;
            let initial_song = self
                .feeluown
                .status()
                .map(|status| (format!("{}{}", status.name, status.singer), status.progress))
                .unwrap_or_default();
            match self.feeluown.play_uri(uri) {
                Ok(_) => {}
                Err(error) => {
                    let message = error.to_string();
                    log::error!("AI点歌播放候选失败: {message}");
                    self.reply(if message.trim().is_empty() {
                        "平台无对应歌曲音源"
                    } else {
                        message.trim()
                    })?;
                    return Ok(PlayOutcome::Error);
                }
            }
            self.confirm_playback_started(
                match_keyword,
                "",
                prefer_accompaniment,
                false,
                false,
                initial_song.0,
                initial_song.1,
                skip_match_check,
            )
        }

        fn play_keyword_confirmed_inner(
            &mut self,
            keyword: &str,
            source: &str,
            prefer_accompaniment: bool,
            allow_switch_source: bool,
            confirm_after_switch: bool,
        ) -> Result<PlayOutcome> {
            self.clear_requested_song_state()?;
            let search_source = if source.trim().is_empty() {
                "qqmusic"
            } else {
                source
            };
            let initial_song = self
                .feeluown
                .status()
                .map(|status| (format!("{}{}", status.name, status.singer), status.progress))
                .unwrap_or_default();

            let result =
                match self
                    .feeluown
                    .play_keyword(keyword, search_source, prefer_accompaniment)
                {
                    Ok(result) => result,
                    Err(error) => {
                        let message = error.to_string();
                        log::error!("点歌搜索失败: {message}");
                        self.reply(if message.trim().is_empty() {
                            "平台无对应歌曲音源"
                        } else {
                            message.trim()
                        })?;
                        return Ok(if message.contains("平台无对应歌曲音源") {
                            PlayOutcome::NoSource
                        } else {
                            PlayOutcome::Error
                        });
                    }
                };
            self.reply(&result.message)?;
            if let Some(candidate) = result.candidate {
                log::info!("FeelUOwn 候选: {} -> {}", candidate.text, candidate.uri);
            }
            self.confirm_playback_started(
                keyword,
                search_source,
                prefer_accompaniment,
                allow_switch_source,
                confirm_after_switch,
                initial_song.0,
                initial_song.1,
                false,
            )
        }

        fn confirm_playback_started(
            &mut self,
            keyword: &str,
            search_source: &str,
            prefer_accompaniment: bool,
            allow_switch_source: bool,
            confirm_after_switch: bool,
            initial_song: String,
            initial_progress: f64,
            skip_match_check: bool,
        ) -> Result<PlayOutcome> {
            sleep(Duration::from_millis(
                self.config.timing.play_search_settle_ms,
            ));

            let mut last_seen_song = initial_song;
            for retry in 0..self.config.timing.play_status_retries {
                let status = match self.feeluown.status() {
                    Ok(status) => status,
                    Err(error) => {
                        log::error!("查询播放状态失败: {error:#}");
                        sleep(Duration::from_millis(
                            self.config.timing.play_status_poll_ms,
                        ));
                        continue;
                    }
                };
                log::info!(
                    "播放状态: {}, 歌曲: {} - {}",
                    status.status,
                    status.name,
                    status.singer
                );
                if status.status != "playing" && status.status != "paused" {
                    sleep(Duration::from_millis(
                        self.config.timing.play_status_poll_ms,
                    ));
                    continue;
                }

                let current_song = format!("{}{}", status.name, status.singer);
                if skip_match_check
                    && !current_song.is_empty()
                    && current_song == last_seen_song
                    && !playback_progress_restarted(initial_progress, status.progress)
                {
                    log::info!(
                        "歌曲未变化，等待 URI 播放生效 ({}/{})",
                        retry + 1,
                        self.config.timing.play_status_retries
                    );
                    sleep(Duration::from_millis(
                        self.config.timing.play_status_poll_ms,
                    ));
                    continue;
                }
                if !skip_match_check {
                    let local_match = song_matcher::match_song_query(
                        &self.config.matching,
                        keyword,
                        &status.name,
                        &status.singer,
                        prefer_accompaniment,
                    );
                    if !local_match.ok {
                        log::info!("歌曲暂不匹配: {}", local_match.reason);
                        if !current_song.is_empty() && current_song == last_seen_song {
                            log::info!(
                                "歌曲未变化，搜索可能尚未完成，继续等待 ({}/{})",
                                retry + 1,
                                self.config.timing.play_status_retries
                            );
                            sleep(Duration::from_millis(
                                self.config.timing.play_status_poll_ms,
                            ));
                            continue;
                        }
                        if !current_song.is_empty() {
                            last_seen_song = current_song.clone();
                        }

                        let mut ai_auto_matched = false;
                        if self.ai.enabled() {
                            match self
                                .ai
                                .match_same_song(keyword, &status.name, &status.singer)
                            {
                                Ok(ai_match) if ai_match.matched => {
                                    log::info!(
                                        "AI自动匹配通过: {} score={}",
                                        ai_match.reason,
                                        ai_match.score
                                    );
                                    match self
                                        .confirm_ai_auto_match(&status, allow_switch_source)?
                                    {
                                        UserDecision::Skip => {
                                            self.report_no_source(Some(&status), true)?;
                                            return Ok(PlayOutcome::NoSource);
                                        }
                                        UserDecision::SwitchSource => {
                                            if allow_switch_source {
                                                return self.switch_source_and_play(
                                                    keyword,
                                                    search_source,
                                                    prefer_accompaniment,
                                                );
                                            }
                                        }
                                        UserDecision::Stopped => return Ok(PlayOutcome::Error),
                                        _ => ai_auto_matched = true,
                                    }
                                }
                                Ok(ai_match) => {
                                    log::info!("AI判断不是同一首: {}", ai_match.reason);
                                }
                                Err(error) => {
                                    log::info!("AI判断异常，回退到人工确认: {error:#}");
                                }
                            }
                        }

                        if !ai_auto_matched {
                            match self.confirm_song(&status, allow_switch_source)? {
                                UserDecision::PromptFailed | UserDecision::Stopped => {
                                    return Ok(PlayOutcome::Error);
                                }
                                UserDecision::SwitchSource => {
                                    if allow_switch_source {
                                        return self.switch_source_and_play(
                                            keyword,
                                            search_source,
                                            prefer_accompaniment,
                                        );
                                    }
                                }
                                UserDecision::Timeout => {
                                    if status.status == "playing" {
                                        let _ = self.feeluown.pause();
                                    }
                                    return Ok(PlayOutcome::NoSource);
                                }
                                UserDecision::Confirm => {}
                                UserDecision::Skip => {
                                    self.report_no_source(Some(&status), true)?;
                                    return Ok(PlayOutcome::NoSource);
                                }
                                _ => {}
                            }
                        }
                    }
                }

                let progress = format_time(status.progress);
                let duration = format_time(status.duration);
                if (progress == "0:00" && duration == "0:00") || duration == "error" {
                    log::info!(
                        "0:00/0:00，等待后重试 ({}/{})",
                        retry + 1,
                        self.config.timing.play_status_retries
                    );
                    sleep(Duration::from_millis(
                        self.config.timing.play_status_poll_ms,
                    ));
                    continue;
                }
                if status.duration > 0.0 && status.duration < 20.0 {
                    log::info!("歌曲时长过短 ({:.1}s)，视为无音源", status.duration);
                    self.report_no_source(Some(&status), true)?;
                    return Ok(PlayOutcome::NoSource);
                }

                if confirm_after_switch {
                    match self.confirm_switched_source_result(&status)? {
                        UserDecision::Skip => {
                            self.report_no_source(Some(&status), true)?;
                            return Ok(PlayOutcome::NoSource);
                        }
                        UserDecision::Stopped => return Ok(PlayOutcome::Error),
                        _ => {}
                    }
                }

                let play_message = format_play_message(&status);
                log::info!("播放成功: {}", play_message);
                self.set_requested_song_state(
                    keyword,
                    search_source,
                    prefer_accompaniment,
                    &status,
                )?;
                self.reply(&play_message)?;
                return Ok(PlayOutcome::Success);
            }

            log::info!("超时未播放成功");
            self.report_no_source(None, false)?;
            Ok(PlayOutcome::NoSource)
        }

        fn switch_source_and_play(
            &mut self,
            keyword: &str,
            current_source: &str,
            prefer_accompaniment: bool,
        ) -> Result<PlayOutcome> {
            let next_source = if current_source == "netease" {
                "qqmusic"
            } else {
                "netease"
            };
            let label = if next_source == "netease" {
                "网易"
            } else {
                "QQ"
            };
            self.reply(&format!("换源到{}: {}", label, keyword))?;
            self.play_keyword_confirmed_inner(
                keyword,
                next_source,
                prefer_accompaniment,
                false,
                true,
            )
        }

        fn confirm_switched_source_result(
            &mut self,
            status: &PlayerStatus,
        ) -> Result<UserDecision> {
            let message = format!(
                "换源结果:{},@确认@跳过",
                song_title(&status.name, &status.singer)
            );
            if self.reply(&message).is_err() {
                return Ok(UserDecision::Timeout);
            }
            self.wait_for_decision(false, false, true)
        }

        fn clear_requested_song_state(&mut self) -> Result<()> {
            let mut runtime_state = self.runtime_state()?;
            runtime_state.state_mut().current_song_is_requested = false;
            runtime_state.state_mut().last_requested_song.clear();
            runtime_state.state_mut().last_requested_keyword.clear();
            runtime_state.state_mut().last_requested_source.clear();
            runtime_state
                .state_mut()
                .last_requested_prefer_accompaniment = false;
            runtime_state.save()
        }

        fn set_requested_song_state(
            &mut self,
            keyword: &str,
            source: &str,
            prefer_accompaniment: bool,
            status: &PlayerStatus,
        ) -> Result<()> {
            let mut runtime_state = self.runtime_state()?;
            runtime_state.state_mut().current_song_is_requested = true;
            runtime_state.state_mut().last_requested_song =
                format!("{}{}", status.name, status.singer);
            runtime_state.state_mut().last_requested_keyword = keyword.to_string();
            runtime_state.state_mut().last_requested_source = source.to_string();
            runtime_state
                .state_mut()
                .last_requested_prefer_accompaniment = prefer_accompaniment;
            runtime_state.state_mut().paused_for_pending_playback = false;
            runtime_state.save()
        }

        fn confirm_song(
            &mut self,
            status: &PlayerStatus,
            allow_switch_source: bool,
        ) -> Result<UserDecision> {
            let actions = if allow_switch_source {
                "@确认@跳过@换源"
            } else {
                "@确认@跳过"
            };
            let message = format!(
                "匹配失败:{},{}",
                song_title(&status.name, &status.singer),
                actions
            );
            if self.reply(&message).is_err() {
                return Ok(UserDecision::PromptFailed);
            }
            self.wait_for_decision(allow_switch_source, false, false)
        }

        fn confirm_ai_auto_match(
            &mut self,
            status: &PlayerStatus,
            allow_switch_source: bool,
        ) -> Result<UserDecision> {
            let actions = if allow_switch_source {
                "@跳过@换源"
            } else {
                "@跳过"
            };
            let message = format!(
                "AI自动匹配:{},如非预期可{}",
                song_title(&status.name, &status.singer),
                actions
            );
            if self.reply(&message).is_err() {
                return Ok(UserDecision::Timeout);
            }
            self.wait_for_decision(allow_switch_source, false, true)
        }

        fn wait_for_decision(
            &mut self,
            allow_switch_source: bool,
            allow_ai: bool,
            timeout_confirms: bool,
        ) -> Result<UserDecision> {
            sleep(Duration::from_millis(
                self.config.timing.post_command_settle_ms,
            ));
            let existing = self.collect_decision_bottoms();
            let deadline =
                Instant::now() + Duration::from_millis(self.config.timing.decision_timeout_ms);
            let template_args = TemplateArgs::default().resolve(&self.config);
            let canvas = Canvas {
                width: self.config.screen.expected_width,
                height: self.config.screen.expected_height,
                resize: true,
            };
            while self.running.load(AtomicOrdering::SeqCst) && Instant::now() < deadline {
                sleep(Duration::from_millis(self.config.timing.decision_poll_ms));
                let frame =
                    match load_frame(&FrameArgs { image: None }, &canvas, &self.config.window) {
                        Ok(frame) => frame,
                        Err(error) => {
                            log::error!("确认命令截图失败: {error:#}");
                            continue;
                        }
                    };
                let scan_result = {
                    let engine = match self.ocr_engine() {
                        Ok(engine) => engine,
                        Err(error) => {
                            log::error!("确认命令 OCR 锁失败: {error:#}");
                            continue;
                        }
                    };
                    scan_chat(
                        &frame.image,
                        &engine.engine,
                        &template_args,
                        self.config.screen.chat_rect.into(),
                    )
                };
                let messages = match scan_result {
                    Ok(messages) => messages,
                    Err(error) => {
                        log::error!("确认命令扫描失败: {error:#}");
                        continue;
                    }
                };
                for message in messages {
                    if message.message_type != "blue" {
                        continue;
                    }
                    if message.text.is_empty()
                        || is_existing_decision(&message, &existing)
                        || is_decision_feedback_text(&message.text)
                    {
                        continue;
                    }
                    match parse_decision_command(&message.text) {
                        Some(UserDecision::Confirm) => return Ok(UserDecision::Confirm),
                        Some(UserDecision::Skip) => return Ok(UserDecision::Skip),
                        Some(UserDecision::SwitchSource) if allow_switch_source => {
                            return Ok(UserDecision::SwitchSource);
                        }
                        Some(UserDecision::Ai) if allow_ai => {
                            return Ok(UserDecision::Ai);
                        }
                        _ => {}
                    }
                }
            }
            if !self.running.load(AtomicOrdering::SeqCst) {
                Ok(UserDecision::Stopped)
            } else if timeout_confirms {
                Ok(UserDecision::Timeout)
            } else {
                self.reply(if allow_switch_source {
                    "此平台匹配失败,命令已超时(20s)下次可以尝试@确认@跳过@换源"
                } else {
                    "此平台匹配失败,命令已超时(20s)下次可以尝试@确认@跳过"
                })?;
                Ok(UserDecision::Timeout)
            }
        }

        fn collect_decision_bottoms(&self) -> HashMap<String, i32> {
            let mut output = HashMap::new();
            let template_args = TemplateArgs::default().resolve(&self.config);
            let canvas = Canvas {
                width: self.config.screen.expected_width,
                height: self.config.screen.expected_height,
                resize: true,
            };
            let Ok(frame) = load_frame(&FrameArgs { image: None }, &canvas, &self.config.window)
            else {
                return output;
            };
            let Ok(engine) = self.ocr_engine() else {
                return output;
            };
            let Ok(messages) = scan_chat(
                &frame.image,
                &engine.engine,
                &template_args,
                self.config.screen.chat_rect.into(),
            ) else {
                return output;
            };
            for message in messages {
                if message.message_type != "blue" {
                    continue;
                }
                if parse_decision_command(&message.text).is_some() {
                    let bottom = message.block.y + message.block.height as i32;
                    output
                        .entry(message.text)
                        .and_modify(|value| *value = (*value).max(bottom))
                        .or_insert(bottom);
                }
            }
            output
        }

        fn report_no_source(
            &self,
            status: Option<&PlayerStatus>,
            pause_playback: bool,
        ) -> Result<()> {
            if pause_playback && status.is_some_and(|status| status.status == "playing") {
                let _ = self.feeluown.pause();
            }
            self.reply("平台无对应歌曲音源")
        }

        fn consume_queue(&mut self, reason: &str) -> Result<()> {
            loop {
                let Some(item) = ({ self.queue()?.front().cloned() }) else {
                    return Ok(());
                };
                log::info!("消费队列({}): {}", reason, item.keyword);
                let request = ResolvedSongRequest {
                    keyword: item.keyword.clone(),
                    source: item.source.clone(),
                    prefer_accompaniment: item.prefer_accompaniment,
                    ai_original_text: item.ai_original_text.clone(),
                    uri: item.uri.clone(),
                    skip_match_check: !item.ai_original_text.trim().is_empty()
                        && !item.uri.trim().is_empty(),
                    friend_username: item.friend_username.clone(),
                };
                let outcome = self.play_request_confirmed(&request, true)?;
                match outcome {
                    PlayOutcome::Success => {
                        self.queue()?.shift()?;
                        return Ok(());
                    }
                    PlayOutcome::NoSource => {
                        self.queue()?.shift()?;
                        log::error!("队列项无音源，已丢弃: {}", item.keyword);
                        continue;
                    }
                    PlayOutcome::Error => {
                        log::error!("队列项播放失败，保留在队首: {}", item.keyword);
                        return Ok(());
                    }
                }
            }
        }

        fn reply(&self, message: &str) -> Result<()> {
            self.chat_output.send(message)
        }

        fn log_queue(&self) -> Result<()> {
            let (len, entries) = {
                let queue = self.queue()?;
                let entries = queue
                    .items()
                    .iter()
                    .enumerate()
                    .map(|(index, item)| format!("{}.{}", index + 1, item.keyword))
                    .collect::<Vec<_>>()
                    .join(", ");
                (queue.len(), entries)
            };
            if len == 0 {
                self.reply("队列为空")?;
            } else {
                self.reply(&format!(
                    "队列({}/{}): {}",
                    len, self.config.queue.max_size, entries
                ))?;
            }
            Ok(())
        }
    }

    fn is_playing(status: &PlayerStatus) -> bool {
        status.status == "playing"
    }

    fn estimated_player_status(snapshot: &PlaybackSnapshot) -> PlayerStatus {
        let mut status = snapshot.status.clone();
        if status.status == "playing" && status.progress.is_finite() {
            status.progress += snapshot.captured_at.elapsed().as_secs_f64();
            if status.duration.is_finite() && status.duration > 0.0 {
                status.progress = status.progress.min(status.duration);
            }
        }
        status
    }

    fn ai_candidate_source(song: &command::SongCommand) -> &'static str {
        if song.friend_username.trim().is_empty() {
            "qqmusic,netease"
        } else {
            song.source.as_str()
        }
    }

    fn song_label(song: &command::SongCommand) -> String {
        source_label(&song.friend_username)
    }

    fn request_label(request: &ResolvedSongRequest) -> String {
        source_label(&request.friend_username)
    }

    fn source_label(username: &str) -> String {
        let username = username.trim();
        if username.is_empty() {
            String::new()
        } else {
            format!("好友{}:", username)
        }
    }

    fn command_log_timestamp() -> String {
        let duration = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let seconds = duration.as_secs() as i64 + 8 * 3600;
        let days = seconds.div_euclid(86_400);
        let second_of_day = seconds.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        let hour = second_of_day / 3600;
        let minute = second_of_day % 3600 / 60;
        let second = second_of_day % 60;
        format!("{year:04}-{month:02}-{day:02}-{hour:02}:{minute:02}:{second:02}")
    }

    fn civil_from_days(days: i64) -> (i64, u32, u32) {
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = mp + if mp < 10 { 3 } else { -9 };
        let year = y + if month <= 2 { 1 } else { 0 };
        (year, month as u32, day as u32)
    }

    fn command_username(parsed: &ParsedCommand) -> &str {
        match &parsed.command {
            UserCommand::Song(song) if !song.friend_username.trim().is_empty() => {
                &song.friend_username
            }
            _ => &parsed.username,
        }
    }

    fn command_location(message_type: &str) -> &str {
        match message_type {
            "pink" => "私聊",
            "blue" => "大厅",
            _ => message_type,
        }
    }

    fn command_log_field(value: &str) -> String {
        value
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .replace('-', "_")
    }

    fn final_song_command_text(request: &ResolvedSongRequest, action: &str) -> String {
        let source = if request.source.trim().is_empty() {
            "all"
        } else {
            request.source.trim()
        };
        format!(
            "{} keyword={} source={} uri={} aiOriginal={}",
            action, request.keyword, source, request.uri, request.ai_original_text,
        )
    }

    fn playback_progress_restarted(before: f64, after: f64) -> bool {
        before.is_finite()
            && after.is_finite()
            && before > 2.0
            && (after < 2.0 || after + 1.0 < before)
    }

    fn parse_decision_command(text: &str) -> Option<UserDecision> {
        let raw = text.trim();
        let command_text = if let Some(index) = raw.find(['：', ':', ']', '】']) {
            let sep_len = raw[index..].chars().next().map(char::len_utf8).unwrap_or(1);
            &raw[index + sep_len..]
        } else {
            raw
        }
        .trim_start_matches(['：', ':', ' ', '\t', ']', '】']);
        if command_text
            .strip_prefix("@确认")
            .is_some_and(|rest| decision_boundary(rest.chars().next()))
        {
            Some(UserDecision::Confirm)
        } else if command_text
            .strip_prefix("@跳过")
            .is_some_and(|rest| decision_boundary(rest.chars().next()))
        {
            Some(UserDecision::Skip)
        } else if command_text
            .strip_prefix("@换源")
            .is_some_and(|rest| decision_boundary(rest.chars().next()))
        {
            Some(UserDecision::SwitchSource)
        } else if command_text
            .strip_prefix("@AI")
            .is_some_and(|rest| decision_boundary(rest.chars().next()))
        {
            Some(UserDecision::Ai)
        } else {
            None
        }
    }

    fn is_decision_feedback_text(text: &str) -> bool {
        [
            "匹配失败",
            "AI自动匹配",
            "换源结果",
            "换源到",
            "换源后仍无音源",
            "下次可以尝试",
            "如非预期",
            "命令已超时",
            "搜索到:",
            "AI匹配:",
            "AI匹配中",
            "AI点歌未启用",
            "AI点歌识别失败",
        ]
        .iter()
        .any(|pattern| text.contains(pattern))
    }

    fn parse_hall_remaining_minutes(text: &str) -> Option<u32> {
        let digits = text
            .chars()
            .filter_map(normalize_ascii_digit)
            .collect::<String>();
        if digits.is_empty() {
            return None;
        }
        let minutes = digits.parse::<u32>().ok()?;
        if (1..=180).contains(&minutes) {
            Some(minutes)
        } else {
            None
        }
    }

    fn merge_hall_info_samples(samples: &[HallInfoSample]) -> HallInfo {
        let name = most_frequent_hall_name(samples).unwrap_or_else(|| {
            samples
                .first()
                .map(|sample| sample.name.clone())
                .unwrap_or_default()
        });
        let is_public_hall = samples
            .iter()
            .filter(|sample| {
                command::normalize_lock_text(&sample.name)
                    == command::normalize_lock_text("公共大厅")
            })
            .count()
            * 2
            >= samples.len().max(1);
        let remaining_minutes = if is_public_hall {
            None
        } else {
            most_frequent_hall_minutes(samples)
        };
        HallInfo {
            name,
            remaining_minutes,
        }
    }

    fn most_frequent_hall_name(samples: &[HallInfoSample]) -> Option<String> {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for name in samples
            .iter()
            .map(|sample| sample.name.trim())
            .filter(|name| !name.is_empty())
        {
            *counts.entry(name.to_string()).or_default() += 1;
        }
        counts
            .into_iter()
            .max_by(|left, right| {
                left.1
                    .cmp(&right.1)
                    .then_with(|| left.0.len().cmp(&right.0.len()))
                    .then_with(|| right.0.cmp(&left.0))
            })
            .map(|(name, _)| name)
    }

    fn most_frequent_hall_minutes(samples: &[HallInfoSample]) -> Option<u32> {
        let mut counts: HashMap<u32, usize> = HashMap::new();
        for minutes in samples.iter().filter_map(|sample| sample.remaining_minutes) {
            *counts.entry(minutes).or_default() += 1;
        }
        counts
            .into_iter()
            .max_by(|left, right| left.1.cmp(&right.1).then_with(|| right.0.cmp(&left.0)))
            .map(|(minutes, _)| minutes)
    }

    fn display_or_empty(text: &str) -> &str {
        if text.is_empty() { "空" } else { text }
    }

    fn normalize_ascii_digit(ch: char) -> Option<char> {
        if ch.is_ascii_digit() {
            return Some(ch);
        }
        if ('\u{ff10}'..='\u{ff19}').contains(&ch) {
            return char::from_u32(ch as u32 - 0xfee0);
        }
        None
    }

    fn format_hall_remaining_suffix(minutes: Option<u32>) -> String {
        minutes
            .map(|value| format!("，剩余{}分钟", value))
            .unwrap_or_default()
    }

    fn parse_invite_decision(text: &str) -> Option<bool> {
        let raw = text.trim();
        let command_text = if let Some(index) = raw.find(['：', ':', ']', '】']) {
            let sep_len = raw[index..].chars().next().map(char::len_utf8).unwrap_or(1);
            &raw[index + sep_len..]
        } else {
            raw
        }
        .trim_start_matches(['：', ':', ' ', '\t', ']', '】']);
        if command_text
            .strip_prefix("@邀请确认")
            .is_some_and(|rest| decision_boundary(rest.chars().next()))
        {
            Some(true)
        } else if command_text
            .strip_prefix("@邀请拒绝")
            .is_some_and(|rest| decision_boundary(rest.chars().next()))
        {
            Some(false)
        } else {
            None
        }
    }

    fn decision_boundary(ch: Option<char>) -> bool {
        match ch {
            None => true,
            Some(ch) => {
                ch.is_whitespace()
                    || matches!(
                        ch,
                        '，' | ',' | '。' | '.' | '!' | '！' | '?' | '？' | ']' | '】'
                    )
            }
        }
    }

    fn is_existing_decision(message: &ChatMessage, existing: &HashMap<String, i32>) -> bool {
        let bottom = message.block.y + message.block.height as i32;
        existing
            .get(&message.text)
            .is_some_and(|existing_bottom| bottom <= *existing_bottom)
    }

    fn format_play_message(status: &PlayerStatus) -> String {
        format!(
            "播放: {} ({}/{}) 音量{}",
            song_title(&status.name, &status.singer),
            format_time(status.progress),
            format_time(status.duration),
            status.volume
        )
    }

    fn song_title(name: &str, singer: &str) -> String {
        let name = if name.trim().is_empty() {
            "未知"
        } else {
            name.trim()
        };
        let singer = if singer.trim().is_empty() {
            "未知"
        } else {
            singer.trim()
        };
        format!("{} - {}", name, singer)
    }

    fn format_time(value: f64) -> String {
        if !value.is_finite() || value <= 0.0 {
            return "0:00".to_string();
        }
        let total = value.floor() as i64;
        format!("{}:{:02}", total / 60, total % 60)
    }

    fn elapsed_ms(started: Instant) -> u128 {
        started.elapsed().as_millis()
    }

    fn load_frame(
        args: &FrameArgs,
        canvas: &Canvas,
        window_config: &config::WindowConfig,
    ) -> Result<Frame> {
        let started = Instant::now();
        let image = match &args.image {
            Some(path) => {
                image::open(path).with_context(|| format!("open image {}", path.display()))?
            }
            None => {
                let _guard = WINDOW_CAPTURE_LOCK
                    .get_or_init(|| Mutex::new(()))
                    .lock()
                    .map_err(|_| anyhow!("window capture mutex poisoned"))?;
                window::capture_game(window_config)?
            }
        };
        let (source_width, source_height) = image.dimensions();
        let image =
            if canvas.resize && (source_width != canvas.width || source_height != canvas.height) {
                image.resize_exact(canvas.width, canvas.height, FilterType::Triangle)
            } else {
                image
            };
        log::debug!(
            "截图加载耗时: {}ms source={}x{} output={}x{} resize={}",
            elapsed_ms(started),
            source_width,
            source_height,
            image.width(),
            image.height(),
            canvas.resize && (source_width != canvas.width || source_height != canvas.height)
        );
        Ok(Frame { image })
    }

    fn scan_chat(
        image: &DynamicImage,
        engine: &OcrEngine,
        templates: &ResolvedTemplateArgs,
        chat_rect: Rect,
    ) -> Result<Vec<ChatMessage>> {
        let total_started = Instant::now();
        let chat = crop_canvas(image, chat_rect)?;
        let marker_started = Instant::now();
        let markers = find_chat_markers(&chat, templates)?;
        let marker_ms = elapsed_ms(marker_started);

        let mut messages = Vec::new();
        let ocr_started = Instant::now();
        for marker in &markers {
            let block = make_message_block(marker, &markers, chat_rect, templates);
            let crop = crop_canvas(&chat, block)?;
            let text = merged_ocr_text(engine, &crop, templates.same_line_y_tolerance)?;
            messages.push(ChatMessage {
                message_type: marker_type(marker).to_string(),
                block,
                text,
            });
        }
        let ocr_ms = elapsed_ms(ocr_started);
        log::info!(
            "聊天扫描耗时: total={}ms marker={}ms ocr={}ms markers={} messages={}",
            elapsed_ms(total_started),
            marker_ms,
            ocr_ms,
            markers.len(),
            messages.len()
        );
        Ok(messages)
    }

    fn marker_type(hit: &TemplateHit) -> &str {
        &hit.kind
    }

    fn find_chat_markers(
        chat: &DynamicImage,
        templates: &ResolvedTemplateArgs,
    ) -> Result<Vec<TemplateHit>> {
        let search_rect = Some(Rect::new(
            0,
            0,
            CHAT_MARKER_SEARCH_WIDTH.min(chat.width()),
            chat.height(),
        ));
        let mut markers = Vec::new();
        markers.extend(find_markers(
            chat,
            search_rect,
            &templates.blue_template,
            "blue",
            templates.marker_threshold,
        )?);
        markers.extend(find_markers(
            chat,
            search_rect,
            &templates.yellow_template,
            "yellow",
            templates.marker_threshold,
        )?);
        markers.extend(find_markers(
            chat,
            search_rect,
            &templates.pink_template,
            "pink",
            templates.marker_threshold,
        )?);
        Ok(dedupe_chat_marker_hits(
            markers,
            templates.marker_dedupe_x,
            templates.marker_dedupe_y,
        ))
    }

    fn chat_marker_counts(markers: &[TemplateHit]) -> ChatMarkerCounts {
        let mut counts = ChatMarkerCounts {
            blue: 0,
            yellow: 0,
            pink: 0,
        };
        for marker in markers {
            match marker.kind.as_str() {
                "blue" => counts.blue += 1,
                "yellow" => counts.yellow += 1,
                "pink" => counts.pink += 1,
                _ => {}
            }
        }
        counts
    }

    fn dedupe_chat_marker_hits(
        hits: Vec<TemplateHit>,
        tolerance_x: i32,
        tolerance_y: i32,
    ) -> Vec<TemplateHit> {
        let tolerance_x = tolerance_x.max(22);
        let tolerance_y = tolerance_y.max(14);
        let mut by_score = hits;
        by_score.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.y.cmp(&right.y))
                .then_with(|| left.x.cmp(&right.x))
        });
        dedupe_hits(by_score, tolerance_x, tolerance_y)
    }

    fn find_markers(
        image: &DynamicImage,
        search_rect: Option<Rect>,
        template: &Path,
        marker_type: &str,
        threshold: f32,
    ) -> Result<Vec<TemplateHit>> {
        let mut hits = find_color_template_hits(image, search_rect, template, threshold)?;
        for hit in &mut hits {
            hit.kind = marker_type.to_string();
        }
        Ok(hits)
    }

    fn make_message_block(
        marker: &TemplateHit,
        markers: &[TemplateHit],
        chat_rect: Rect,
        templates: &ResolvedTemplateArgs,
    ) -> Rect {
        let start_y = clamp_i32(
            marker.y - templates.block_top_padding,
            0,
            chat_rect.height as i32 - 1,
        );
        let next_marker = next_marker(marker, markers, templates.next_marker_min_gap);
        let max_end_y = clamp_i32(
            start_y + templates.max_block_height,
            start_y + 1,
            chat_rect.height as i32,
        );
        let boundary_end_y = next_marker
            .map(|hit| hit.y - templates.block_bottom_padding)
            .unwrap_or(max_end_y);
        let end_y = clamp_i32(
            boundary_end_y.min(max_end_y),
            start_y + 1,
            chat_rect.height as i32,
        );
        let text_x = clamp_i32(
            marker.x + marker.width as i32 + templates.text_left_gap,
            0,
            chat_rect.width as i32 - 1,
        );
        let width = clamp_i32(
            chat_rect.width as i32 - text_x - templates.right_padding,
            1,
            chat_rect.width as i32,
        ) as u32;
        Rect::new(text_x, start_y, width, (end_y - start_y) as u32)
    }

    fn next_marker<'a>(
        marker: &TemplateHit,
        markers: &'a [TemplateHit],
        next_marker_min_gap: i32,
    ) -> Option<&'a TemplateHit> {
        let min_y = marker.y + next_marker_min_gap.max((marker.height as f32 * 0.6).floor() as i32);
        markers
            .iter()
            .filter(|candidate| candidate.y >= min_y)
            .min_by_key(|candidate| candidate.y)
    }

    fn detect_ui_state(
        image: &DynamicImage,
        templates: &ResolvedUiTemplateArgs,
        screen: &config::ScreenConfig,
    ) -> Result<UiState> {
        let started = Instant::now();
        if best_template_hit(
            image,
            Some(screen.enter_rect.into()),
            &templates.enter_template,
            templates.chat_templates.marker_threshold,
        )?
        .is_some()
        {
            log::debug!(
                "UI 状态检测耗时: {}ms state=primary_enter",
                elapsed_ms(started)
            );
            return Ok(UiState::primary_enter());
        }

        if best_template_hit(
            image,
            Some(screen.secondary_hall_rect.into()),
            &templates.dating_template,
            templates.chat_templates.marker_threshold,
        )?
        .is_some()
        {
            log::debug!(
                "UI 状态检测耗时: {}ms state=secondary_hall",
                elapsed_ms(started)
            );
            return Ok(UiState::secondary_hall());
        }

        let chat = crop_canvas(image, screen.chat_rect.into())?;
        let marker_counts =
            chat_marker_counts(&find_chat_markers(&chat, &templates.chat_templates)?);
        let blue = marker_counts.blue;
        let yellow = marker_counts.yellow;
        let pink = marker_counts.pink;
        if blue + yellow + pink > 0 {
            log::debug!(
                "UI 状态检测耗时: {}ms state=primary_marker blue={} yellow={} pink={}",
                elapsed_ms(started),
                blue,
                yellow,
                pink
            );
            return Ok(UiState::primary_marker(blue, yellow, pink));
        }

        log::debug!("UI 状态检测耗时: {}ms state=unknown", elapsed_ms(started));
        Ok(UiState::unknown())
    }

    fn find_template_hits(
        image: &DynamicImage,
        search_rect: Option<Rect>,
        template_path: &Path,
        threshold: f32,
    ) -> Result<Vec<TemplateHit>> {
        let haystack = match search_rect {
            Some(rect) => crop_canvas(image, rect)?,
            None => image.clone(),
        };
        let template = image::open(template_path)
            .with_context(|| format!("open template {}", template_path.display()))?;

        if template.width() > haystack.width() || template.height() > haystack.height() {
            return Ok(Vec::new());
        }

        let haystack_gray = haystack.to_luma8();
        let template_gray = template.to_luma8();
        let haystack_match = to_match_image(&haystack_gray);
        let template_match = to_match_image(&template_gray);
        let result = match_template(
            haystack_match,
            template_match,
            MatchTemplateMethod::SumOfAbsoluteDifferences,
        );
        let max_sad = (template_gray.width() * template_gray.height()).max(1) as f32;
        let mut hits = Vec::new();
        for y in 0..result.height {
            for x in 0..result.width {
                let idx = (y * result.width + x) as usize;
                let score = 1.0 - result.data[idx] / max_sad;
                if score >= threshold {
                    let base_x = search_rect.map(|rect| rect.x).unwrap_or(0);
                    let base_y = search_rect.map(|rect| rect.y).unwrap_or(0);
                    hits.push(TemplateHit {
                        kind: "template".to_string(),
                        x: base_x + x as i32,
                        y: base_y + y as i32,
                        width: template_gray.width(),
                        height: template_gray.height(),
                        score,
                    });
                }
            }
        }
        hits.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.y.cmp(&right.y))
                .then_with(|| left.x.cmp(&right.x))
        });
        Ok(dedupe_hits(
            hits,
            template_gray.width().max(8) as i32 / 2,
            template_gray.height().max(8) as i32 / 2,
        ))
    }

    fn find_color_template_hits(
        image: &DynamicImage,
        search_rect: Option<Rect>,
        template_path: &Path,
        threshold: f32,
    ) -> Result<Vec<TemplateHit>> {
        let haystack = match search_rect {
            Some(rect) => crop_canvas(image, rect)?,
            None => image.clone(),
        };
        let template_rgb = cached_rgb_template(template_path)?;

        if template_rgb.width() > haystack.width() || template_rgb.height() > haystack.height() {
            return Ok(Vec::new());
        }

        let haystack_rgb = haystack.to_rgb8();
        let max_sad = template_rgb.width() as u64 * template_rgb.height() as u64 * 3 * 255;
        let max_allowed_sad = ((1.0 - threshold).clamp(0.0, 1.0) * max_sad as f32) as u64;
        let mut hits = Vec::new();

        for y in 0..=(haystack_rgb.height() - template_rgb.height()) {
            for x in 0..=(haystack_rgb.width() - template_rgb.width()) {
                let sad = color_sad_at(&haystack_rgb, &template_rgb, x, y, max_allowed_sad);
                let score = 1.0 - sad as f32 / max_sad as f32;
                if score >= threshold {
                    let base_x = search_rect.map(|rect| rect.x).unwrap_or(0);
                    let base_y = search_rect.map(|rect| rect.y).unwrap_or(0);
                    hits.push(TemplateHit {
                        kind: "template".to_string(),
                        x: base_x + x as i32,
                        y: base_y + y as i32,
                        width: template_rgb.width(),
                        height: template_rgb.height(),
                        score,
                    });
                }
            }
        }
        hits.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.y.cmp(&right.y))
                .then_with(|| left.x.cmp(&right.x))
        });
        Ok(dedupe_hits(
            hits,
            template_rgb.width().max(8) as i32 / 2,
            template_rgb.height().max(8) as i32 / 2,
        ))
    }

    fn cached_rgb_template(template_path: &Path) -> Result<RgbImage> {
        let cache = RGB_TEMPLATE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        let mut cache = cache
            .lock()
            .map_err(|_| anyhow!("template cache mutex poisoned"))?;
        if let Some(image) = cache.get(template_path) {
            return Ok(image.clone());
        }
        let image = image::open(template_path)
            .with_context(|| format!("open template {}", template_path.display()))?
            .to_rgb8();
        cache.insert(template_path.to_path_buf(), image.clone());
        Ok(image)
    }

    fn color_sad_at(
        haystack: &RgbImage,
        template: &RgbImage,
        x: u32,
        y: u32,
        max_allowed_sad: u64,
    ) -> u64 {
        let haystack_width = haystack.width() as usize;
        let template_width = template.width() as usize;
        let template_height = template.height() as usize;
        let x = x as usize;
        let y = y as usize;
        let haystack_data = haystack.as_raw();
        let template_data = template.as_raw();
        let mut sad = 0u64;

        for row in 0..template_height {
            let haystack_offset = ((y + row) * haystack_width + x) * 3;
            let template_offset = row * template_width * 3;
            for channel in 0..(template_width * 3) {
                sad += haystack_data[haystack_offset + channel]
                    .abs_diff(template_data[template_offset + channel])
                    as u64;
                if sad > max_allowed_sad {
                    return sad;
                }
            }
        }
        sad
    }

    fn best_template_hit(
        image: &DynamicImage,
        search_rect: Option<Rect>,
        template_path: &Path,
        threshold: f32,
    ) -> Result<Option<TemplateHit>> {
        let haystack = match search_rect {
            Some(rect) => crop_canvas(image, rect)?,
            None => image.clone(),
        };
        let template = image::open(template_path)
            .with_context(|| format!("open template {}", template_path.display()))?;
        if template.width() > haystack.width() || template.height() > haystack.height() {
            return Ok(None);
        }

        let haystack_gray = haystack.to_luma8();
        let template_gray = template.to_luma8();
        let result = match_template(
            to_match_image(&haystack_gray),
            to_match_image(&template_gray),
            MatchTemplateMethod::SumOfAbsoluteDifferences,
        );
        let extremes = find_extremes(&result);
        let max_sad = (template_gray.width() * template_gray.height()).max(1) as f32;
        let score = 1.0 - extremes.min_value / max_sad;
        if score < threshold {
            return Ok(None);
        }
        let base_x = search_rect.map(|rect| rect.x).unwrap_or(0);
        let base_y = search_rect.map(|rect| rect.y).unwrap_or(0);
        Ok(Some(TemplateHit {
            kind: "template".to_string(),
            x: base_x + extremes.min_value_location.0 as i32,
            y: base_y + extremes.min_value_location.1 as i32,
            width: template_gray.width(),
            height: template_gray.height(),
            score,
        }))
    }

    fn to_match_image(image: &GrayImage) -> MatchImage<'static> {
        let data = image
            .pixels()
            .map(|pixel| pixel.0[0] as f32 / 255.0)
            .collect::<Vec<_>>();
        MatchImage::new(data, image.width(), image.height())
    }

    fn dedupe_hits(
        mut hits: Vec<TemplateHit>,
        tolerance_x: i32,
        tolerance_y: i32,
    ) -> Vec<TemplateHit> {
        let mut picked: Vec<TemplateHit> = Vec::new();
        for hit in hits.drain(..) {
            if picked.iter().any(|picked| {
                (hit.x - picked.x).abs() <= tolerance_x && (hit.y - picked.y).abs() <= tolerance_y
            }) {
                continue;
            }
            picked.push(hit);
        }
        picked.sort_by(compare_hits_top_left);
        picked
    }

    fn crop_canvas(image: &DynamicImage, rect: Rect) -> Result<DynamicImage> {
        if rect.x < 0
            || rect.y < 0
            || rect.right() > image.width() as i32
            || rect.bottom() > image.height() as i32
        {
            bail!(
                "crop rect {},{},{},{} outside image {}x{}",
                rect.x,
                rect.y,
                rect.width,
                rect.height,
                image.width(),
                image.height()
            );
        }
        Ok(image.crop_imm(rect.x as u32, rect.y as u32, rect.width, rect.height))
    }

    pub(super) fn configured_chat_change_fingerprint(
        image: &DynamicImage,
        chat_rect: RectConfig,
    ) -> Result<ChangeFingerprint> {
        chat_change_fingerprint(image, chat_rect.into())
    }

    fn rect_chat_change_fingerprint(image: &DynamicImage, rect: Rect) -> Result<ChangeFingerprint> {
        chat_change_fingerprint(image, rect)
    }

    pub(super) fn count_chat_markers(
        image: &DynamicImage,
        templates: &ResolvedTemplateArgs,
        chat_rect: RectConfig,
    ) -> Result<(usize, usize, usize)> {
        let chat = crop_canvas(image, chat_rect.into())?;
        let markers = find_chat_markers(&chat, templates)?;
        let counts = chat_marker_counts(&markers);
        Ok((counts.blue, counts.yellow, counts.pink))
    }

    fn chat_change_fingerprint(image: &DynamicImage, chat_rect: Rect) -> Result<ChangeFingerprint> {
        const WIDTH: u32 = 104;
        const HEIGHT: u32 = 36;

        let chat = crop_canvas(image, chat_rect)?;
        let gray = chat
            .resize_exact(WIDTH, HEIGHT, FilterType::Triangle)
            .to_luma8();
        Ok(ChangeFingerprint {
            pixels: gray.into_raw(),
            width: WIDTH,
            height: HEIGHT,
        })
    }

    pub(super) fn change_stats(
        previous: &ChangeFingerprint,
        current: &ChangeFingerprint,
    ) -> ChangeStats {
        if previous.width != current.width
            || previous.height != current.height
            || previous.pixels.len() != current.pixels.len()
        {
            return ChangeStats {
                mean_abs_diff: f32::MAX,
                changed_ratio: 1.0,
            };
        }

        let mut total_diff = 0u64;
        let mut changed = 0usize;
        for (left, right) in previous.pixels.iter().zip(&current.pixels) {
            let diff = left.abs_diff(*right);
            total_diff += diff as u64;
            if diff >= 12 {
                changed += 1;
            }
        }
        let count = previous.pixels.len().max(1);
        ChangeStats {
            mean_abs_diff: total_diff as f32 / count as f32,
            changed_ratio: changed as f32 / count as f32,
        }
    }

    fn click_game_point(point: PointConfig, window_config: &config::WindowConfig) -> Result<()> {
        let mut enigo = Enigo::new(&Settings::default()).context("create enigo")?;
        let mut window = window::GameWindow::find(window_config)?;
        window.click(&mut enigo, point)?;
        Ok(())
    }

    fn press_key(key: Key, window_config: &config::WindowConfig) -> Result<()> {
        let mut enigo = Enigo::new(&Settings::default()).context("create enigo")?;
        let mut window = window::GameWindow::find(window_config)?;
        window.focus_for_keyboard(&mut enigo)?;
        enigo.key(key, Direction::Click).context("press key")?;
        Ok(())
    }

    fn run_or_print<F>(execute: bool, description: String, action: F) -> Result<()>
    where
        F: FnOnce() -> Result<()>,
    {
        if execute {
            action()
        } else {
            println!("dry-run: {}", description);
            println!("pass --execute to send real keyboard/mouse input");
            Ok(())
        }
    }

    fn parse_key(value: &str) -> Result<Key> {
        let normalized = value.trim().to_ascii_lowercase();
        let key = match normalized.as_str() {
            "return" | "enter" => Key::Return,
            "escape" | "esc" => Key::Escape,
            "f1" => Key::F1,
            "f2" => Key::F2,
            "f3" => Key::F3,
            "f4" => Key::F4,
            "f5" => Key::F5,
            "f6" => Key::F6,
            "f7" => Key::F7,
            "f8" => Key::F8,
            "f9" => Key::F9,
            "f10" => Key::F10,
            "f11" => Key::F11,
            "f12" => Key::F12,
            "n" => Key::Unicode('n'),
            single if single.chars().count() == 1 => Key::Unicode(single.chars().next().unwrap()),
            _ => bail!("unsupported key: {}", value),
        };
        Ok(key)
    }

    fn parse_rect(value: &str) -> Result<Rect> {
        let parts = value
            .split(',')
            .map(str::trim)
            .map(str::parse::<i32>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if parts.len() != 4 {
            bail!("rect must be x,y,width,height");
        }
        if parts[2] <= 0 || parts[3] <= 0 {
            bail!("rect width and height must be positive");
        }
        Ok(Rect::new(
            parts[0],
            parts[1],
            parts[2] as u32,
            parts[3] as u32,
        ))
    }

    fn compare_hits_top_left(left: &TemplateHit, right: &TemplateHit) -> Ordering {
        (left.y / 10)
            .cmp(&(right.y / 10))
            .then_with(|| left.x.cmp(&right.x))
            .then_with(|| left.y.cmp(&right.y))
    }

    fn clamp_i32(value: i32, min: i32, max: i32) -> i32 {
        value.max(min).min(max)
    }

    fn print_json<T: Serialize>(value: &T) -> Result<()> {
        println!("{}", serde_json::to_string_pretty(value)?);
        Ok(())
    }
}

#[cfg(target_os = "windows")]
fn main() -> anyhow::Result<()> {
    app::run()
}
