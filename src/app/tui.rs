use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Console::{
    CONSOLE_MODE, ENABLE_EXTENDED_FLAGS, ENABLE_INSERT_MODE, ENABLE_QUICK_EDIT_MODE,
    GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode,
};

use super::config::TuiConfig;
use super::monitor::{MonitorQueueItem, MonitorShared, MonitorSnapshot};

pub(super) struct TuiHandle {
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    console_mode_guard: Option<ConsoleInputModeGuard>,
}

type TuiTerminal = Terminal<CrosstermBackend<io::Stdout>>;

struct ConsoleInputModeGuard {
    handle: HANDLE,
    original_mode: CONSOLE_MODE,
}

impl TuiHandle {
    pub(super) fn start(config: &TuiConfig, shared: MonitorShared) -> io::Result<Self> {
        let console_mode_guard = ConsoleInputModeGuard::disable_quick_edit_and_insert();
        let running = Arc::new(AtomicBool::new(true));
        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = Terminal::new(backend)?;
        terminal.clear()?;
        let thread_shared = shared.clone();
        let thread_running = Arc::clone(&running);
        let refresh = Duration::from_millis(config.refresh_ms.max(33));
        let thread =
            thread::spawn(move || render_loop(terminal, thread_shared, thread_running, refresh));
        Ok(Self {
            running,
            thread: Some(thread),
            console_mode_guard,
        })
    }
}

impl ConsoleInputModeGuard {
    fn disable_quick_edit_and_insert() -> Option<Self> {
        let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) }.ok()?;
        let mut original_mode = CONSOLE_MODE(0);
        unsafe { GetConsoleMode(handle, &mut original_mode) }.ok()?;
        let new_mode = CONSOLE_MODE(
            (original_mode.0 | ENABLE_EXTENDED_FLAGS.0)
                & !ENABLE_QUICK_EDIT_MODE.0
                & !ENABLE_INSERT_MODE.0,
        );
        unsafe { SetConsoleMode(handle, new_mode) }.ok()?;
        Some(Self {
            handle,
            original_mode,
        })
    }
}

impl Drop for ConsoleInputModeGuard {
    fn drop(&mut self) {
        let _ = unsafe { SetConsoleMode(self.handle, self.original_mode) };
    }
}

impl Drop for TuiHandle {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        drop(self.console_mode_guard.take());
    }
}

fn render_loop(
    mut terminal: TuiTerminal,
    shared: MonitorShared,
    running: Arc<AtomicBool>,
    refresh: Duration,
) {
    while running.load(Ordering::SeqCst) {
        let snapshot = shared.snapshot();
        let _ = terminal.draw(|frame| draw(frame, &snapshot));
        thread::sleep(refresh);
    }
    let _ = terminal.show_cursor();
}

fn draw(frame: &mut ratatui::Frame<'_>, state: &MonitorSnapshot) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),
            Constraint::Length(8),
            Constraint::Length(3),
        ])
        .split(frame.area());

    let dashboard = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(40),
            Constraint::Percentage(30),
            Constraint::Percentage(30),
        ])
        .split(chunks[1]);

    let log_lines = state
        .logs
        .iter()
        .rev()
        .take(chunks[0].height.saturating_sub(2) as usize)
        .rev()
        .map(|line| Line::from(line.as_str()))
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(log_lines)
            .block(Block::default().title(" 实时日志 ").borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
        chunks[0],
    );

    let ocr_lines = match &state.ocr {
        Some(ocr) => {
            let mut lines = vec![Line::from(vec![
                Span::styled("markers", Style::default().fg(Color::Cyan)),
                Span::raw(format!(": {}  ", ocr.markers)),
                Span::styled("耗时", Style::default().fg(Color::Cyan)),
                Span::raw(format!(
                    ": total={}ms marker={}ms ocr={}ms",
                    ocr.total_ms, ocr.marker_ms, ocr.ocr_ms
                )),
            ])];
            lines.extend(
                ocr.messages
                    .iter()
                    .map(|message| Line::from(message.as_str())),
            );
            lines
        }
        None => vec![Line::from("暂无 OCR 内容")],
    };
    frame.render_widget(
        Paragraph::new(ocr_lines)
            .block(Block::default().title(" OCR 内容 ").borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
        dashboard[0],
    );

    let queue_lines = if state.queue.is_empty() {
        vec![Line::from("队列为空")]
    } else {
        state
            .queue
            .iter()
            .map(|item| Line::from(format_queue_item(item)))
            .collect::<Vec<_>>()
    };
    frame.render_widget(
        Paragraph::new(queue_lines)
            .block(Block::default().title(" 队列 ").borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
        dashboard[1],
    );

    let command_lines = if state.commands.is_empty() {
        vec![Line::from("暂无命令")]
    } else {
        state
            .commands
            .iter()
            .rev()
            .take(dashboard[2].height.saturating_sub(2) as usize)
            .rev()
            .map(|command| Line::from(command.as_str()))
            .collect::<Vec<_>>()
    };
    frame.render_widget(
        Paragraph::new(command_lines)
            .block(Block::default().title(" 命令 ").borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
        dashboard[2],
    );

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" 状态 ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(state.status.as_str()),
        ]))
        .block(Block::default().borders(Borders::ALL)),
        chunks[2],
    );
}

fn format_queue_item(item: &MonitorQueueItem) -> String {
    let mut text = String::new();
    if !item.friend_username.trim().is_empty() {
        text.push_str(&item.friend_username);
        text.push_str(": ");
    }
    text.push_str(&item.keyword);
    if !item.source.trim().is_empty() {
        text.push_str(" [");
        text.push_str(&item.source);
        text.push(']');
    }
    if item.prefer_accompaniment {
        text.push_str(" 伴奏");
    }
    text
}
