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

use crate::{now_ms, AmpHistory, ChatBuffer, Contacts, MAX_LOG_LINES};

enum Screen {
    Connect,
    Main,
}

enum InputMode {
    Ptt,
    Message,
}

enum ConnectTab {
    Dial,
    Listen,
}

enum ConnectFocus {
    NodeId,
    Alias,
    Contacts,
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
    peer_id_tx: tokio::sync::oneshot::Sender<Option<(NodeId, Option<String>)>>,
    contacts: Contacts,
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
    let mut connect_tab = ConnectTab::Dial;
    let mut connect_node_id = String::new();
    let mut connect_alias = String::new();
    let mut connect_focus = ConnectFocus::NodeId;
    let mut contact_selected: usize = 0;
    let mut connect_error: Option<String> = None;
    let mut chat_input = String::new();

    loop {
        let _ = terminal.draw(|frame| {
            let area = frame.area();
            match screen {
                Screen::Connect => {
                    let contacts_snap: Vec<(String, String)> = contacts.lock().unwrap()
                        .iter()
                        .map(|c| (c.alias.clone(), c.node_id.clone()))
                        .collect();
                    draw_connect(
                        frame,
                        area,
                        &connect_node_id,
                        &connect_alias,
                        &connect_focus,
                        &connect_tab,
                        connect_error.as_deref(),
                        &node_id,
                        &contacts_snap,
                        contact_selected,
                    );
                }
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
                        let contacts_snap: Vec<(String, String)> = contacts.lock().unwrap()
                            .iter()
                            .map(|c| (c.alias.clone(), c.node_id.clone()))
                            .collect();
                        handle_connect_key(
                            key,
                            &mut connect_node_id,
                            &mut connect_alias,
                            &mut connect_focus,
                            &mut connect_tab,
                            &mut connect_error,
                            &mut peer_id_tx,
                            &mut shutdown_tx,
                            &mut screen,
                            &contacts_snap,
                            &mut contact_selected,
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

#[allow(clippy::too_many_arguments)]
fn draw_connect(
    frame: &mut ratatui::Frame,
    area: Rect,
    node_id_input: &str,
    alias_input: &str,
    focus: &ConnectFocus,
    tab: &ConnectTab,
    error: Option<&str>,
    node_id: &str,
    contacts: &[(String, String)],
    contact_selected: usize,
) {
    let visible = if matches!(tab, ConnectTab::Dial) { contacts.len().min(4) } else { 0 };
    let popup_height = match tab {
        ConnectTab::Dial => 13 + if visible > 0 { 1 + visible as u16 } else { 0 },
        ConnectTab::Listen => 9,
    };
    let popup = centered_rect(area, 72, popup_height);

    let tab_bar = Line::from(vec![
        Span::raw("  "),
        if matches!(tab, ConnectTab::Dial) {
            Span::styled("Connect", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD | Modifier::UNDERLINED))
        } else {
            Span::styled("Connect", Style::default().fg(Color::DarkGray))
        },
        Span::styled("   │   ", Style::default().fg(Color::DarkGray)),
        if matches!(tab, ConnectTab::Listen) {
            Span::styled("Listen", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD | Modifier::UNDERLINED))
        } else {
            Span::styled("Listen", Style::default().fg(Color::DarkGray))
        },
        Span::styled("                 Tab to switch", Style::default().fg(Color::DarkGray)),
    ]);

    let sep = "  ─────────────────────────────────────────────";
    let your_id = Line::from(vec![
        Span::styled("  Your ID: ", Style::default().fg(Color::DarkGray)),
        Span::styled(node_id.to_owned(), Style::default().fg(Color::Cyan)),
    ]);

    let mut content = vec![
        tab_bar,
        Line::from(Span::styled(sep, Style::default().fg(Color::DarkGray))),
    ];

    match tab {
        ConnectTab::Dial => {
            let peer_label = if let Some(err) = error {
                Line::from(vec![
                    Span::styled("  Error: ", Style::default().fg(Color::Red)),
                    Span::raw(err.to_owned()),
                ])
            } else {
                Line::from(Span::styled("  Peer ID:", Style::default().fg(Color::White)))
            };
            let peer_input = match focus {
                ConnectFocus::NodeId => Line::from(vec![
                    Span::styled("  > ", Style::default().fg(Color::Yellow)),
                    Span::raw(node_id_input.to_owned()),
                    Span::styled("█", Style::default().fg(Color::Yellow)),
                ]),
                _ => Line::from(Span::styled(
                    format!("  {}", node_id_input),
                    if node_id_input.is_empty() { Style::default().fg(Color::DarkGray) } else { Style::default().fg(Color::White) },
                )),
            };
            let alias_input_line = match focus {
                ConnectFocus::Alias => Line::from(vec![
                    Span::styled("  > ", Style::default().fg(Color::Yellow)),
                    Span::raw(alias_input.to_owned()),
                    Span::styled("█", Style::default().fg(Color::Yellow)),
                ]),
                _ => if alias_input.is_empty() {
                    Line::from(Span::styled("  (optional)", Style::default().fg(Color::DarkGray)))
                } else {
                    Line::from(Span::raw(format!("  {}", alias_input)))
                },
            };
            content.extend([
                peer_label,
                peer_input,
                Line::from(""),
                Line::from(Span::styled("  Save as:", Style::default().fg(Color::White))),
                alias_input_line,
                Line::from(""),
                Line::from(Span::styled("  ↑↓: switch field   Enter: dial", Style::default().fg(Color::DarkGray))),
            ]);
            if visible > 0 {
                content.push(Line::from(Span::styled(
                    format!("{}  saved", sep),
                    Style::default().fg(Color::DarkGray),
                )));
                let start = if contact_selected >= 4 { contact_selected - 3 } else { 0 };
                for (i, (alias, nid)) in contacts[start..start + visible].iter().enumerate() {
                    let abs_i = start + i;
                    let short_nid: String = nid.chars().take(16).collect();
                    let is_sel = matches!(focus, ConnectFocus::Contacts) && abs_i == contact_selected;
                    if is_sel {
                        content.push(Line::from(Span::styled(
                            format!("▶ {:<12} {}…", alias, short_nid),
                            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                        )));
                    } else {
                        content.push(Line::from(Span::styled(
                            format!("  {:<12} {}…", alias, short_nid),
                            Style::default().fg(Color::White),
                        )));
                    }
                }
            }
        }
        ConnectTab::Listen => {
            content.extend([
                Line::from(""),
                your_id,
                Line::from(""),
                Line::from(Span::styled("  Press Enter to start listening.", Style::default().fg(Color::DarkGray))),
            ]);
        }
    }

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

#[allow(clippy::too_many_arguments)]
fn handle_connect_key(
    key: crossterm::event::KeyEvent,
    node_id_input: &mut String,
    alias_input: &mut String,
    focus: &mut ConnectFocus,
    tab: &mut ConnectTab,
    connect_error: &mut Option<String>,
    peer_id_tx: &mut Option<tokio::sync::oneshot::Sender<Option<(NodeId, Option<String>)>>>,
    shutdown_tx: &mut Option<tokio::sync::oneshot::Sender<()>>,
    screen: &mut Screen,
    contacts: &[(String, String)],
    contact_selected: &mut usize,
) {
    if !contacts.is_empty() {
        *contact_selected = (*contact_selected).min(contacts.len() - 1);
    }
    if key.kind == KeyEventKind::Release {
        return;
    }
    match key.code {
        KeyCode::Tab => {
            *tab = match tab {
                ConnectTab::Dial => ConnectTab::Listen,
                ConnectTab::Listen => {
                    *focus = ConnectFocus::NodeId;
                    ConnectTab::Dial
                }
            };
        }
        KeyCode::Up if matches!(tab, ConnectTab::Dial) => {
            match focus {
                ConnectFocus::Contacts => {
                    if *contact_selected > 0 {
                        *contact_selected -= 1;
                    } else {
                        *focus = ConnectFocus::Alias;
                    }
                }
                ConnectFocus::Alias => { *focus = ConnectFocus::NodeId; }
                ConnectFocus::NodeId => {}
            }
        }
        KeyCode::Down if matches!(tab, ConnectTab::Dial) => {
            match focus {
                ConnectFocus::NodeId => { *focus = ConnectFocus::Alias; }
                ConnectFocus::Alias => {
                    if !contacts.is_empty() {
                        *focus = ConnectFocus::Contacts;
                    }
                }
                ConnectFocus::Contacts => {
                    if !contacts.is_empty() && *contact_selected < contacts.len() - 1 {
                        *contact_selected += 1;
                    }
                }
            }
        }
        KeyCode::Enter => match tab {
            ConnectTab::Listen => {
                if let Some(tx) = peer_id_tx.take() {
                    let _ = tx.send(None);
                }
                *screen = Screen::Main;
            }
            ConnectTab::Dial => match focus {
                ConnectFocus::Contacts => {
                    if let Some((alias, nid)) = contacts.get(*contact_selected) {
                        *node_id_input = nid.clone();
                        *alias_input = alias.clone();
                        *focus = ConnectFocus::NodeId;
                    }
                }
                _ => {
                    let trimmed = node_id_input.trim().to_owned();
                    if trimmed.is_empty() {
                        if let Some(tx) = peer_id_tx.take() {
                            let _ = tx.send(None);
                        }
                        *screen = Screen::Main;
                    } else {
                        match trimmed.parse::<NodeId>() {
                            Ok(id) => {
                                let alias_opt = {
                                    let a = alias_input.trim().to_owned();
                                    if a.is_empty() { None } else { Some(a) }
                                };
                                if let Some(tx) = peer_id_tx.take() {
                                    let _ = tx.send(Some((id, alias_opt)));
                                }
                                *connect_error = None;
                                node_id_input.clear();
                                alias_input.clear();
                                *screen = Screen::Main;
                            }
                            Err(_) => {
                                *connect_error = Some(
                                    "not a valid node ID (expected 52-char base32)".into(),
                                );
                            }
                        }
                    }
                }
            },
        },
        KeyCode::Backspace if matches!(tab, ConnectTab::Dial) => {
            match focus {
                ConnectFocus::NodeId => { node_id_input.pop(); }
                ConnectFocus::Alias => { alias_input.pop(); }
                ConnectFocus::Contacts => {}
            }
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
        KeyCode::Char(c)
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(tab, ConnectTab::Dial) =>
        {
            match focus {
                ConnectFocus::NodeId => { node_id_input.push(c); }
                ConnectFocus::Alias => { alias_input.push(c); }
                ConnectFocus::Contacts => {}
            }
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
