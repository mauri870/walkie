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
    layout::{Constraint, Direction, Layout},
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
    let mut input_buf = String::new();
    let mut input_submitted = false;
    let mut input_error: Option<String> = None;

    loop {
        let _ = terminal.draw(|frame| {
            let area = frame.area();
            let mut constraints = vec![
                Constraint::Min(1),
                Constraint::Length(5),
                Constraint::Length(5),
            ];
            if !input_submitted {
                constraints.push(Constraint::Length(3));
            }
            constraints.push(Constraint::Length(4));
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints(constraints)
                .split(area);

            // Log panel
            let log_height = chunks[0].height.saturating_sub(2) as usize;
            let items: Vec<ListItem> = {
                let logs = logs.0.lock().unwrap();
                let skip = logs.len().saturating_sub(log_height);
                logs.iter()
                    .skip(skip)
                    .map(|l| ListItem::new(l.clone()))
                    .collect()
            };
            frame.render_widget(
                List::new(items)
                    .block(Block::default().borders(Borders::ALL).title(" Logs ")),
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

            // Input panel — visible until the user submits
            if !input_submitted {
                let input_line = if let Some(err) = &input_error {
                    Line::from(vec![
                        Span::styled("Invalid ID: ", Style::default().fg(Color::Red)),
                        Span::raw(err.clone()),
                    ])
                } else {
                    Line::from(vec![
                        Span::raw(input_buf.clone()),
                        Span::styled("█", Style::default().fg(Color::Yellow)),
                    ])
                };
                frame.render_widget(
                    Paragraph::new(input_line).block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(" Enter peer node ID (press Enter to listen) "),
                    ),
                    chunks[3],
                );
            }

            // Status bar
            let status_idx = if input_submitted { 3 } else { 4 };
            let ptt_active = ptt.load(Ordering::Relaxed);
            let (ptt_label, ptt_style) = if ptt_active {
                (
                    "● TRANSMITTING",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )
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
                        Span::raw(format!("  │  Node: {}", node_id)),
                        {
                            let us = ping_us.load(Ordering::Relaxed);
                            if us == 0 {
                                Span::styled(
                                    "  │  Ping: --",
                                    Style::default().fg(Color::DarkGray),
                                )
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
                chunks[status_idx],
            );
        });

        if event::poll(Duration::from_millis(50)).unwrap_or(false) {
            if let Ok(Event::Key(key)) = event::read() {
                if !input_submitted {
                    match key.code {
                        KeyCode::Enter if key.kind != KeyEventKind::Release => {
                            let trimmed = input_buf.trim().to_owned();
                            if trimmed.is_empty() {
                                if let Some(tx) = peer_id_tx.take() {
                                    let _ = tx.send(None);
                                }
                                input_submitted = true;
                            } else {
                                match trimmed.parse::<NodeId>() {
                                    Ok(id) => {
                                        if let Some(tx) = peer_id_tx.take() {
                                            let _ = tx.send(Some(id));
                                        }
                                        input_submitted = true;
                                        input_error = None;
                                    }
                                    Err(e) => {
                                        input_error = Some(e.to_string());
                                        input_buf.clear();
                                    }
                                }
                            }
                        }
                        KeyCode::Backspace if key.kind != KeyEventKind::Release => {
                            input_buf.pop();
                            input_error = None;
                        }
                        KeyCode::Char(c)
                            if key.kind != KeyEventKind::Release
                                && !key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            input_buf.push(c);
                            input_error = None;
                        }
                        KeyCode::Char('c')
                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            if let Some(tx) = peer_id_tx.take() {
                                let _ = tx.send(None);
                            }
                            if let Some(tx) = shutdown_tx.take() {
                                let _ = tx.send(());
                            }
                            break;
                        }
                        _ => {}
                    }
                } else {
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
                        KeyCode::Char('q') => {
                            if let Some(tx) = shutdown_tx.take() {
                                let _ = tx.send(());
                            }
                            break;
                        }
                        KeyCode::Char('c')
                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            if let Some(tx) = shutdown_tx.take() {
                                let _ = tx.send(());
                            }
                            break;
                        }
                        _ => {}
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
