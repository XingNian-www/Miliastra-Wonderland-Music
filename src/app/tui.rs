use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect as TuiRect};
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

const STATUS_HEIGHT: u16 = 3;
const MAX_OCR_MESSAGES: usize = 5;
const MAX_QUEUE_ITEMS: usize = 5;
const OCR_PANEL_HEIGHT: u16 = MAX_OCR_MESSAGES as u16 + 3;
const QUEUE_PANEL_HEIGHT: u16 = MAX_QUEUE_ITEMS as u16 + 2;
const MIN_COMMAND_HEIGHT: u16 = 5;
const MIN_NARROW_DASHBOARD_HEIGHT: u16 = OCR_PANEL_HEIGHT + QUEUE_PANEL_HEIGHT + 3;

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
    let area = frame.area();
    let log_height = event_log_height(area.height);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(8),
            Constraint::Length(log_height),
            Constraint::Length(STATUS_HEIGHT),
        ])
        .split(area);

    draw_dashboard(frame, chunks[0], state);
    draw_event_log(frame, chunks[1], state);
    draw_status(frame, chunks[2], state);
}

fn draw_dashboard(frame: &mut ratatui::Frame<'_>, area: TuiRect, state: &MonitorSnapshot) {
    if area.width >= 72 {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(ocr_queue_row_height(area.height)),
                Constraint::Min(MIN_COMMAND_HEIGHT),
            ])
            .split(area);
        let top = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
            .split(rows[0]);
        draw_ocr(frame, top[0], state);
        draw_queue(frame, top[1], state);
        draw_commands(frame, rows[1], state);
    } else {
        let ocr_height = stacked_ocr_height(area.height);
        let queue_height = stacked_queue_height(area.height.saturating_sub(ocr_height));
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(ocr_height),
                Constraint::Length(queue_height),
                Constraint::Min(3),
            ])
            .split(area);
        draw_ocr(frame, rows[0], state);
        draw_queue(frame, rows[1], state);
        draw_commands(frame, rows[2], state);
    }
}

fn draw_event_log(frame: &mut ratatui::Frame<'_>, area: TuiRect, state: &MonitorSnapshot) {
    let log_lines = state
        .logs
        .iter()
        .rev()
        .take(area.height.saturating_sub(2) as usize)
        .rev()
        .map(|line| Line::from(line.as_str()))
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(log_lines)
            .block(Block::default().title(" 事件日志 ").borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_ocr(frame: &mut ratatui::Frame<'_>, area: TuiRect, state: &MonitorSnapshot) {
    let ocr_lines = match &state.ocr {
        Some(ocr) => {
            let mut lines = vec![Line::from(vec![
                Span::styled("标记", Style::default().fg(Color::Cyan)),
                Span::raw(format!(": {}  ", ocr.markers)),
                Span::styled("耗时", Style::default().fg(Color::Cyan)),
                Span::raw(format!(
                    ": total={}ms marker={}ms OCR={}ms",
                    ocr.total_ms, ocr.marker_ms, ocr.ocr_ms
                )),
            ])];
            lines.extend(
                recent_ocr_messages(&ocr.messages).map(|message| Line::from(message.as_str())),
            );
            lines
        }
        None => vec![Line::from("暂无 OCR 内容")],
    };
    let title = state.ocr.as_ref().map_or_else(
        || " OCR 内容 ".to_string(),
        |ocr| {
            format!(
                " OCR 内容 {}/{} ",
                ocr.messages.len().min(MAX_OCR_MESSAGES),
                ocr.messages.len()
            )
        },
    );
    frame.render_widget(
        Paragraph::new(ocr_lines).block(Block::default().title(title).borders(Borders::ALL)),
        area,
    );
}

fn draw_queue(frame: &mut ratatui::Frame<'_>, area: TuiRect, state: &MonitorSnapshot) {
    let queue_lines = if state.queue.is_empty() {
        vec![Line::from("队列为空")]
    } else {
        state
            .queue
            .iter()
            .take(MAX_QUEUE_ITEMS)
            .map(|item| Line::from(format_queue_item(item)))
            .collect::<Vec<_>>()
    };
    let title = if state.queue.is_empty() {
        " 队列 ".to_string()
    } else {
        format!(
            " 队列 {}/{} ",
            state.queue.len().min(MAX_QUEUE_ITEMS),
            state.queue.len()
        )
    };
    frame.render_widget(
        Paragraph::new(queue_lines).block(Block::default().title(title).borders(Borders::ALL)),
        area,
    );
}

fn draw_commands(frame: &mut ratatui::Frame<'_>, area: TuiRect, state: &MonitorSnapshot) {
    let command_lines = if state.commands.is_empty() {
        vec![Line::from("暂无命令")]
    } else {
        state
            .commands
            .iter()
            .rev()
            .take(area.height.saturating_sub(2) as usize)
            .rev()
            .map(|command| Line::from(command.as_str()))
            .collect::<Vec<_>>()
    };
    frame.render_widget(
        Paragraph::new(command_lines)
            .block(Block::default().title(" 命令 ").borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_status(frame: &mut ratatui::Frame<'_>, area: TuiRect, state: &MonitorSnapshot) {
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" 状态 ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(state.status.as_str()),
            Span::styled("  监听 ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(state.chat_listener.mode.as_str()),
            Span::raw(if state.chat_listener.pending_mode.is_empty() {
                String::new()
            } else {
                format!(" (等待切换{})", state.chat_listener.pending_mode)
            }),
        ]))
        .block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn event_log_height(total_height: u16) -> u16 {
    let desired = ((total_height as u32 * 35) / 100) as u16;
    let max_height = total_height
        .saturating_sub(STATUS_HEIGHT + MIN_NARROW_DASHBOARD_HEIGHT)
        .max(3);
    desired.clamp(8, 18).min(max_height)
}

fn ocr_queue_row_height(total_height: u16) -> u16 {
    OCR_PANEL_HEIGHT
        .max(QUEUE_PANEL_HEIGHT)
        .min(total_height.saturating_sub(MIN_COMMAND_HEIGHT).max(3))
}

fn stacked_ocr_height(total_height: u16) -> u16 {
    OCR_PANEL_HEIGHT.min(total_height.saturating_sub(QUEUE_PANEL_HEIGHT + 3).max(3))
}

fn stacked_queue_height(total_height: u16) -> u16 {
    QUEUE_PANEL_HEIGHT.min(total_height.saturating_sub(3).max(3))
}

fn recent_ocr_messages(messages: &[String]) -> impl DoubleEndedIterator<Item = &String> {
    messages
        .iter()
        .rev()
        .take(MAX_OCR_MESSAGES)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_log_gets_a_taller_default_area() {
        assert!(event_log_height(40) >= 13);
        assert!(event_log_height(43) >= 15);
    }

    #[test]
    fn ocr_and_queue_panels_match_five_visible_rows() {
        assert_eq!(OCR_PANEL_HEIGHT, 8);
        assert_eq!(QUEUE_PANEL_HEIGHT, 7);
        assert_eq!(ocr_queue_row_height(30), 8);
    }

    #[test]
    fn ocr_panel_keeps_latest_five_messages() {
        let messages = (1..=7)
            .map(|index| format!("消息{index}"))
            .collect::<Vec<_>>();
        let visible = recent_ocr_messages(&messages)
            .map(String::as_str)
            .collect::<Vec<_>>();

        assert_eq!(visible, vec!["消息3", "消息4", "消息5", "消息6", "消息7"]);
    }
}
