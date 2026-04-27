use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use iroh::NodeId;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Sparkline},
    Terminal,
};

use crate::{now_ms, AmpHistory, MAX_LOG_LINES};

// -- Log capture --

#[derive(Clone)]
pub(crate) struct LogBuffer(pub(crate) Arc<Mutex<VecDeque<String>>>);

impl LogBuffer {
    pub(crate) fn new() -> Self {
        Self(Arc::new(Mutex::new(VecDeque::new())))
    }
}

pub(crate) struct LogWriter(LogBuffer);

impl std::io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(s) = std::str::from_utf8(buf) {
            for line in s.lines() {
                if !line.is_empty() {
                    let mut logs = self.0 .0.lock().unwrap();
                    if logs.len() >= MAX_LOG_LINES {
                        logs.pop_front();
                    }
                    logs.push_back(line.to_string());
                }
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = LogWriter;
    fn make_writer(&'a self) -> Self::Writer {
        LogWriter(self.clone())
    }
}

// -- TUI --

enum Screen {
    Connect,
    Main,
}

fn centered_rect(area: Rect, width_pct: u16, height: u16) -> Rect {
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(height),
            Constraint::Min(1),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_pct) / 2),
            Constraint::Percentage(width_pct),
            Constraint::Percentage((100 - width_pct) / 2),
        ])
        .split(vert[1])[1]
}

pub(crate) fn run_tui(
    logs: LogBuffer,
    ptt: Arc<AtomicBool>,
    ptt_last: Arc<AtomicU64>,
    ping_us: Arc<AtomicU64>,
    mic_amp: AmpHistory,
    audio_amp: AmpHistory,
    node_id: String,
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
    running: Arc<AtomicBool>,
    peer_id_tx: tokio::sync::oneshot::Sender<Option<NodeId>>,
) {
    terminal::enable_raw_mode().unwrap();
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen).unwrap();

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).unwrap();

    let mut shutdown_tx = Some(shutdown_tx);
    let mut peer_id_tx = Some(peer_id_tx);

    let mut screen = Screen::Connect;
    let mut connect_input = String::new();
    let mut connect_error: Option<String> = None;

    loop {
        let _ = terminal.draw(|frame| {
            let area = frame.area();
            match screen {
                Screen::Connect => draw_connect(
                    frame,
                    area,
                    &connect_input,
                    connect_error.as_deref(),
                    &node_id,
                ),
                Screen::Main => draw_main(
                    frame,
                    area,
                    &logs,
                    &ptt,
                    &ping_us,
                    &mic_amp,
                    &audio_amp,
                    &node_id,
                ),
            }
        });

        if event::poll(Duration::from_millis(50)).unwrap_or(false) {
            if let Ok(Event::Key(key)) = event::read() {
                match screen {
                    Screen::Connect => {
                        handle_connect_key(
                            key,
                            &mut connect_input,
                            &mut connect_error,
                            &mut peer_id_tx,
                            &mut shutdown_tx,
                            &mut screen,
                        );
                        if shutdown_tx.is_none() {
                            break;
                        }
                    }
                    Screen::Main => {
                        if handle_main_key(key, &ptt, &ptt_last, &mut shutdown_tx) {
                            break;
                        }
                    }
                }
            }
        }

        if !running.load(Ordering::Relaxed) {
            break;
        }
    }

    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
}

fn draw_connect(
    frame: &mut ratatui::Frame,
    area: Rect,
    input: &str,
    error: Option<&str>,
    node_id: &str,
) {
    let popup = centered_rect(area, 70, 9);

    let input_line = if let Some(err) = error {
        Line::from(vec![
            Span::styled("  Invalid ID: ", Style::default().fg(Color::Red)),
            Span::raw(err.to_owned()),
        ])
    } else {
        Line::from(vec![
            Span::styled("  > ", Style::default().fg(Color::Yellow)),
            Span::raw(input.to_owned()),
            Span::styled("█", Style::default().fg(Color::Yellow)),
        ])
    };

    let content = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  Enter peer node ID to connect,",
            Style::default().fg(Color::White),
        )),
        Line::from(Span::styled(
            "  or press Enter to listen only:",
            Style::default().fg(Color::White),
        )),
        Line::from(""),
        input_line,
        Line::from(""),
        Line::from(vec![
            Span::styled("  Your ID: ", Style::default().fg(Color::DarkGray)),
            Span::styled(node_id.to_owned(), Style::default().fg(Color::Cyan)),
        ]),
    ];

    frame.render_widget(
        Paragraph::new(content)
            .block(Block::default().borders(Borders::ALL).title(" walkie ")),
        popup,
    );
}

fn draw_main(
    frame: &mut ratatui::Frame,
    area: Rect,
    logs: &LogBuffer,
    ptt: &Arc<AtomicBool>,
    ping_us: &Arc<AtomicU64>,
    mic_amp: &AmpHistory,
    audio_amp: &AmpHistory,
    node_id: &str,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(5),
            Constraint::Length(5),
            Constraint::Length(4),
        ])
        .split(area);

    // Log panel
    let log_height = chunks[0].height.saturating_sub(2) as usize;
    let items: Vec<ListItem> = {
        let log_lines = logs.0.lock().unwrap();
        let skip = log_lines.len().saturating_sub(log_height);
        log_lines
            .iter()
            .skip(skip)
            .map(|l| ListItem::new(l.clone()))
            .collect()
    };
    frame.render_widget(
        List::new(items).block(Block::default().borders(Borders::ALL).title(" Logs ")),
        chunks[0],
    );

    // Mic sparkline
    let mic_data: Vec<u64> = mic_amp.lock().unwrap().iter().copied().collect();
    frame.render_widget(
        Sparkline::default()
            .block(Block::default().borders(Borders::ALL).title(" Mic "))
            .data(&mic_data)
            .style(Style::default().fg(Color::Green)),
        chunks[1],
    );

    // Audio sparkline
    let audio_data: Vec<u64> = audio_amp.lock().unwrap().iter().copied().collect();
    frame.render_widget(
        Sparkline::default()
            .block(Block::default().borders(Borders::ALL).title(" Audio "))
            .data(&audio_data)
            .style(Style::default().fg(Color::Yellow)),
        chunks[2],
    );

    // Status bar
    let ptt_active = ptt.load(Ordering::Relaxed);
    let (ptt_label, ptt_style) = if ptt_active {
        ("● TRANSMITTING", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
    } else {
        ("● LISTENING", Style::default().fg(Color::Green))
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "SPACE: push to talk   q / ctrl+c: quit",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(vec![
                Span::styled(ptt_label, ptt_style),
                Span::raw(format!("  │  Node: {node_id}")),
                {
                    let us = ping_us.load(Ordering::Relaxed);
                    if us == 0 {
                        Span::styled("  │  Ping: --", Style::default().fg(Color::DarkGray))
                    } else {
                        Span::styled(
                            format!("  │  Ping: {:.1}ms", us as f64 / 1000.0),
                            Style::default().fg(Color::Cyan),
                        )
                    }
                },
            ]),
        ])
        .block(Block::default().borders(Borders::ALL).title(" Status ")),
        chunks[3],
    );
}

fn handle_connect_key(
    key: crossterm::event::KeyEvent,
    connect_input: &mut String,
    connect_error: &mut Option<String>,
    peer_id_tx: &mut Option<tokio::sync::oneshot::Sender<Option<NodeId>>>,
    shutdown_tx: &mut Option<tokio::sync::oneshot::Sender<()>>,
    screen: &mut Screen,
) {
    if key.kind == KeyEventKind::Release {
        return;
    }
    match key.code {
        KeyCode::Enter => {
            let trimmed = connect_input.trim().to_owned();
            if trimmed.is_empty() {
                if let Some(tx) = peer_id_tx.take() {
                    let _ = tx.send(None);
                }
                *screen = Screen::Main;
            } else {
                match trimmed.parse::<NodeId>() {
                    Ok(id) => {
                        if let Some(tx) = peer_id_tx.take() {
                            let _ = tx.send(Some(id));
                        }
                        *connect_error = None;
                        connect_input.clear();
                        *screen = Screen::Main;
                    }
                    Err(e) => {
                        *connect_error = Some(e.to_string());
                        connect_input.clear();
                    }
                }
            }
        }
        KeyCode::Backspace => {
            connect_input.pop();
            *connect_error = None;
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(tx) = peer_id_tx.take() {
                let _ = tx.send(None);
            }
            if let Some(tx) = shutdown_tx.take() {
                let _ = tx.send(());
            }
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            connect_input.push(c);
            *connect_error = None;
        }
        _ => {}
    }
}

fn handle_main_key(
    key: crossterm::event::KeyEvent,
    ptt: &Arc<AtomicBool>,
    ptt_last: &Arc<AtomicU64>,
    shutdown_tx: &mut Option<tokio::sync::oneshot::Sender<()>>,
) -> bool {
    match key.code {
        KeyCode::Char(' ') => {
            if key.kind == KeyEventKind::Release {
                ptt_last.store(0, Ordering::Relaxed);
                ptt.store(false, Ordering::Relaxed);
            } else {
                ptt_last.store(now_ms(), Ordering::Relaxed);
                ptt.store(true, Ordering::Relaxed);
            }
        }
        KeyCode::Char('q') if key.kind != KeyEventKind::Release => {
            if let Some(tx) = shutdown_tx.take() {
                let _ = tx.send(());
            }
            return true;
        }
        KeyCode::Char('c')
            if key.kind != KeyEventKind::Release
                && key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            if let Some(tx) = shutdown_tx.take() {
                let _ = tx.send(());
            }
            return true;
        }
        _ => {}
    }
    false
}
