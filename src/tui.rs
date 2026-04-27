use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
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
use tokio::sync::broadcast;

use crate::{now_ms, AmpHistory, ChatBuffer, MAX_LOG_LINES};

enum Screen {
    Connect,
    Main,
}

enum InputMode {
    Ptt,
    Message,
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
    chat_in: ChatBuffer,
    chat_out_tx: broadcast::Sender<String>,
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
    let mut input_mode = InputMode::Ptt;
    let mut connect_input = String::new();
    let mut connect_error: Option<String> = None;
    let mut chat_input = String::new();

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
                    &chat_in,
                    &chat_input,
                    &input_mode,
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
                        let quit = handle_main_key(
                            key,
                            &mut input_mode,
                            &mut chat_input,
                            &chat_in,
                            &chat_out_tx,
                            &ptt,
                            &ptt_last,
                            &mut shutdown_tx,
                        );
                        if quit {
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

#[allow(clippy::too_many_arguments)]
fn draw_main(
    frame: &mut ratatui::Frame,
    area: Rect,
    chat_in: &ChatBuffer,
    chat_input: &str,
    input_mode: &InputMode,
    ptt: &Arc<AtomicBool>,
    ping_us: &Arc<AtomicU64>,
    mic_amp: &AmpHistory,
    audio_amp: &AmpHistory,
    node_id: &str,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),
            Constraint::Length(5),
            Constraint::Length(5),
            Constraint::Length(4),
        ])
        .split(area);

    // Chat block with embedded input at the bottom
    let mode_label = match input_mode {
        InputMode::Ptt => "PTT",
        InputMode::Message => "MSG",
    };
    let chat_block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Chat [{mode_label}] "));
    let chat_inner = chat_block.inner(chunks[0]);
    frame.render_widget(chat_block, chunks[0]);

    // Split inner area: messages | separator | input line
    let inner = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(chat_inner);

    let msg_height = inner[0].height as usize;
    let items: Vec<ListItem> = {
        let messages = chat_in.lock().unwrap();
        let skip = messages.len().saturating_sub(msg_height);
        messages
            .iter()
            .skip(skip)
            .map(|l| ListItem::new(l.clone()))
            .collect()
    };
    frame.render_widget(List::new(items), inner[0]);

    let sep_width = inner[1].width as usize;
    frame.render_widget(
        Paragraph::new(Span::styled(
            "─".repeat(sep_width),
            Style::default().fg(Color::DarkGray),
        )),
        inner[1],
    );

    let input_line = match input_mode {
        InputMode::Message => Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Yellow)),
            Span::raw(chat_input.to_owned()),
            Span::styled("█", Style::default().fg(Color::Yellow)),
        ]),
        InputMode::Ptt => Line::from(Span::styled(
            "> Tab: enter message mode",
            Style::default().fg(Color::DarkGray),
        )),
    };
    frame.render_widget(Paragraph::new(input_line), inner[2]);

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
    let hints = match input_mode {
        InputMode::Ptt => "Tab: message mode   SPACE: push to talk   q / ctrl+c: quit",
        InputMode::Message => "Tab: PTT mode   Enter: send   ctrl+c: quit",
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(hints, Style::default().fg(Color::DarkGray))),
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

#[allow(clippy::too_many_arguments)]
fn handle_main_key(
    key: crossterm::event::KeyEvent,
    input_mode: &mut InputMode,
    chat_input: &mut String,
    chat_in: &ChatBuffer,
    chat_out_tx: &broadcast::Sender<String>,
    ptt: &Arc<AtomicBool>,
    ptt_last: &Arc<AtomicU64>,
    shutdown_tx: &mut Option<tokio::sync::oneshot::Sender<()>>,
) -> bool {
    match input_mode {
        InputMode::Message => {
            if key.kind == KeyEventKind::Release {
                return false;
            }
            match key.code {
                KeyCode::Tab => *input_mode = InputMode::Ptt,
                KeyCode::Enter => {
                    let text = chat_input.trim().to_owned();
                    if !text.is_empty() {
                        let mut buf = chat_in.lock().unwrap();
                        if buf.len() >= MAX_LOG_LINES {
                            buf.pop_front();
                        }
                        buf.push_back(format!("[you] {text}"));
                        drop(buf);
                        let _ = chat_out_tx.send(text);
                        chat_input.clear();
                    }
                }
                KeyCode::Backspace => {
                    chat_input.pop();
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(tx) = shutdown_tx.take() {
                        let _ = tx.send(());
                    }
                    return true;
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    chat_input.push(c);
                }
                _ => {}
            }
        }
        InputMode::Ptt => {
            match key.code {
                KeyCode::Tab if key.kind != KeyEventKind::Release => {
                    *input_mode = InputMode::Message;
                }
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
        }
    }
    false
}
