use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use super::config::TuiConfig;

#[derive(Clone, Debug)]
pub(super) struct OcrSnapshot {
    pub(super) markers: usize,
    pub(super) messages: Vec<String>,
    pub(super) marker_ms: u128,
    pub(super) ocr_ms: u128,
    pub(super) total_ms: u128,
}

#[derive(Clone)]
pub(super) struct TuiShared {
    state: Arc<Mutex<TuiState>>,
}

#[derive(Clone)]
pub(super) struct TuiLogSink {
    shared: TuiShared,
}

pub(super) struct TuiHandle {
    shared: TuiShared,
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

type TuiTerminal = Terminal<CrosstermBackend<io::Stdout>>;

#[derive(Debug)]
struct TuiState {
    logs: VecDeque<String>,
    log_limit: usize,
    ocr: Option<OcrSnapshot>,
    status: String,
}

impl TuiShared {
    fn new(log_limit: usize) -> Self {
        Self {
            state: Arc::new(Mutex::new(TuiState {
                logs: VecDeque::new(),
                log_limit: log_limit.max(20),
                ocr: None,
                status: "启动中".to_string(),
            })),
        }
    }

    pub(super) fn push_log(&self, line: String) {
        if let Ok(mut state) = self.state.lock() {
            let mut pushed = false;
            for part in line.lines() {
                state.logs.push_back(part.to_string());
                pushed = true;
                while state.logs.len() > state.log_limit {
                    state.logs.pop_front();
                }
            }
            if !pushed {
                state.logs.push_back(String::new());
                while state.logs.len() > state.log_limit {
                    state.logs.pop_front();
                }
            }
        }
    }

    pub(super) fn set_ocr(&self, snapshot: OcrSnapshot) {
        if let Ok(mut state) = self.state.lock() {
            state.ocr = Some(snapshot);
        }
    }

    pub(super) fn set_status(&self, status: impl Into<String>) {
        if let Ok(mut state) = self.state.lock() {
            state.status = status.into();
        }
    }
}

impl TuiLogSink {
    pub(super) fn push(&self, line: String) {
        self.shared.push_log(line);
    }
}

impl TuiHandle {
    pub(super) fn start(config: &TuiConfig) -> io::Result<Self> {
        let shared = TuiShared::new(config.log_lines);
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
            shared,
            running,
            thread: Some(thread),
        })
    }

    pub(super) fn shared(&self) -> TuiShared {
        self.shared.clone()
    }

    pub(super) fn log_sink(&self) -> TuiLogSink {
        TuiLogSink {
            shared: self.shared.clone(),
        }
    }
}

impl Drop for TuiHandle {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn render_loop(
    mut terminal: TuiTerminal,
    shared: TuiShared,
    running: Arc<AtomicBool>,
    refresh: Duration,
) {
    while running.load(Ordering::SeqCst) {
        let snapshot = shared.state.lock().ok().map(|state| RenderState {
            logs: state.logs.iter().cloned().collect(),
            ocr: state.ocr.clone(),
            status: state.status.clone(),
        });
        if let Some(snapshot) = snapshot {
            let _ = terminal.draw(|frame| draw(frame, &snapshot));
        }
        thread::sleep(refresh);
    }
    let _ = terminal.show_cursor();
}

#[derive(Debug)]
struct RenderState {
    logs: Vec<String>,
    ocr: Option<OcrSnapshot>,
    status: String,
}

fn draw(frame: &mut ratatui::Frame<'_>, state: &RenderState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(58),
            Constraint::Percentage(37),
            Constraint::Length(3),
        ])
        .split(frame.area());

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
        chunks[1],
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
