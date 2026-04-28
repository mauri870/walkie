mod audio;
mod transport;
mod tui;

use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait};
use iroh::{Endpoint, NodeId, SecretKey};
use tokio::sync::broadcast;
use tracing_subscriber::EnvFilter;

use tui::run_tui;
use transport::handle_conn;

// -- Shared constants and types --

pub(crate) const SAMPLE_RATE: u32 = 48_000;
pub(crate) const FRAME_SIZE: usize = 960; // 20 ms at 48 kHz
pub(crate) const AMP_HISTORY_LEN: usize = 150;
pub(crate) const PTT_TIMEOUT_MS: u64 = 300;
pub(crate) const MAX_LOG_LINES: usize = 200;

pub(crate) type AmpHistory = Arc<Mutex<VecDeque<u64>>>;
pub(crate) type PlaybackBuf = Arc<Mutex<VecDeque<f32>>>;
pub(crate) type ChatBuffer = Arc<Mutex<VecDeque<String>>>;

pub(crate) struct Contact {
    pub alias: String,
    pub node_id: String,
}

pub(crate) type Contacts = Arc<Mutex<Vec<Contact>>>;

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// -- Arg parsing --

struct Args {
    list_devices: bool,
    input_device: Option<String>,
    output_device: Option<String>,
}

fn parse_args() -> Result<Args> {
    let mut args = std::env::args().skip(1);
    let mut list_devices = false;
    let mut input_device = None;
    let mut output_device = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--list-devices" => list_devices = true,
            "--input-device" => {
                input_device = Some(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--input-device requires a value"))?,
                );
            }
            "--output-device" => {
                output_device = Some(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--output-device requires a value"))?,
                );
            }
            other => anyhow::bail!(
                "unknown argument: {other}\n\nUsage: walkie [--list-devices] [--input-device <NAME>] [--output-device <NAME>]"
            ),
        }
    }
    Ok(Args { list_devices, input_device, output_device })
}

// -- Identity --

fn load_or_create_secret_key() -> Result<SecretKey> {
    let key_path = dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("walkie")
        .join("secret.key");

    if key_path.exists() {
        let hex = std::fs::read_to_string(&key_path)?;
        Ok(hex.trim().parse::<SecretKey>()?)
    } else {
        let key = SecretKey::generate(&mut rand::rngs::OsRng);
        std::fs::create_dir_all(key_path.parent().unwrap())?;
        std::fs::write(&key_path, key.to_string())?;
        key_path.to_str().map(|p| eprintln!("Identity saved to {p}"));
        Ok(key)
    }
}

// -- Contacts --

fn contacts_path() -> std::path::PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("walkie")
        .join("aliases")
}

fn load_contacts() -> Vec<Contact> {
    let path = contacts_path();
    if !path.exists() {
        return Vec::new();
    }
    std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let (alias, node_id) = line.split_once('=')?;
            let alias = alias.trim().to_owned();
            let node_id = node_id.trim().to_owned();
            if alias.is_empty() || node_id.is_empty() {
                return None;
            }
            Some(Contact { alias, node_id })
        })
        .collect()
}

fn save_contacts(contacts: &[Contact]) {
    let path = contacts_path();
    let content = contacts
        .iter()
        .map(|c| format!("{}={}", c.alias, c.node_id))
        .collect::<Vec<_>>()
        .join("\n");
    let _ = std::fs::write(path, content);
}

fn upsert_contact(contacts: &mut Vec<Contact>, alias: String, node_id: String) {
    if let Some(c) = contacts.iter_mut().find(|c| c.node_id == node_id) {
        c.alias = alias;
    } else {
        contacts.push(Contact { alias, node_id });
    }
}

// -- Main --

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args()?;

    let log_path = dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("walkie")
        .join("walkie.log");
    std::fs::create_dir_all(log_path.parent().unwrap())?;
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    tracing_subscriber::fmt()
        .compact()
        .with_ansi(false)
        .with_writer(Mutex::new(log_file))
        .with_env_filter(EnvFilter::new("warn,iroh_net_report=error"))
        .init();

    if args.list_devices {
        let host = cpal::default_host();
        println!("Input devices:");
        for d in host.input_devices()? {
            println!("  {}", d.name()?);
        }
        println!("Output devices:");
        for d in host.output_devices()? {
            println!("  {}", d.name()?);
        }
        return Ok(());
    }

    let secret_key = load_or_create_secret_key()?;
    let endpoint = Endpoint::builder()
        .secret_key(secret_key)
        .discovery_n0()
        .alpns(vec![iroh_roq::ALPN.to_vec()])
        .bind()
        .await?;

    let node_id = endpoint.node_id();

    let ptt = Arc::new(AtomicBool::new(false));
    let ptt_last = Arc::new(AtomicU64::new(0));
    let ping_us = Arc::new(AtomicU64::new(0));
    let mic_amp: AmpHistory = Arc::new(Mutex::new(VecDeque::new()));
    let audio_amp: AmpHistory = Arc::new(Mutex::new(VecDeque::new()));
    let chat_in: ChatBuffer = Arc::new(Mutex::new(VecDeque::new()));
    let (chat_out_tx, _) = broadcast::channel::<String>(32);
    let contacts: Contacts = Arc::new(Mutex::new(load_contacts()));

    // Accept loop
    tokio::spawn({
        let endpoint = endpoint.clone();
        let ptt = ptt.clone();
        let ping_us = ping_us.clone();
        let mic_amp = mic_amp.clone();
        let audio_amp = audio_amp.clone();
        let input_device = args.input_device.clone();
        let output_device = args.output_device.clone();
        let chat_in = chat_in.clone();
        let chat_out_tx = chat_out_tx.clone();
        let contacts = contacts.clone();
        async move {
            loop {
                match endpoint.accept().await {
                    Some(incoming) => match incoming.await {
                        Ok(conn) => {
                            let ptt = ptt.clone();
                            let ping_us = ping_us.clone();
                            let mic_amp = mic_amp.clone();
                            let audio_amp = audio_amp.clone();
                            let input_device = input_device.clone();
                            let output_device = output_device.clone();
                            let chat_in = chat_in.clone();
                            let chat_out_tx = chat_out_tx.clone();
                            let contacts_ref = contacts.clone();
                            tokio::spawn(async move {
                                let peer_alias = conn.remote_node_id().ok().and_then(|id| {
                                    let id_str = id.to_string();
                                    contacts_ref.lock().unwrap()
                                        .iter()
                                        .find(|c| c.node_id == id_str)
                                        .map(|c| c.alias.clone())
                                });
                                if let Err(e) = handle_conn(
                                    conn, ptt, ping_us, mic_amp, audio_amp,
                                    input_device, output_device,
                                    chat_in, chat_out_tx, peer_alias,
                                )
                                .await
                                {
                                    tracing::warn!("connection closed: {e}");
                                }
                            });
                        }
                        Err(e) => tracing::warn!("accept error: {e}"),
                    },
                    None => break,
                }
            }
        }
    });

    // PTT watchdog
    let ptt_wd = ptt.clone();
    let ptt_last_wd = ptt_last.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(50));
        loop {
            interval.tick().await;
            let t = ptt_last_wd.load(Ordering::Relaxed);
            if t > 0 && now_ms().saturating_sub(t) >= PTT_TIMEOUT_MS {
                ptt_last_wd.store(0, Ordering::Relaxed);
                ptt_wd.store(false, Ordering::Relaxed);
            }
        }
    });

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let (peer_id_tx, peer_id_rx) = tokio::sync::oneshot::channel::<Option<(NodeId, Option<String>)>>();
    let running = Arc::new(AtomicBool::new(true));

    let tui_thread = {
        let running = running.clone();
        let ptt = ptt.clone();
        let ping_us = ping_us.clone();
        let mic_amp = mic_amp.clone();
        let audio_amp = audio_amp.clone();
        let chat_in = chat_in.clone();
        let chat_out_tx = chat_out_tx.clone();
        let contacts = contacts.clone();
        std::thread::spawn(move || {
            run_tui(
                chat_in,
                chat_out_tx,
                ptt,
                ptt_last,
                ping_us,
                mic_amp,
                audio_amp,
                node_id.to_string(),
                shutdown_tx,
                running,
                peer_id_tx,
                contacts,
            );
        })
    };

    // Wait for peer ID from TUI, then connect
    if let Ok(Some((peer_id, peer_alias))) = peer_id_rx.await {
        if let Some(alias) = &peer_alias {
            let mut guard = contacts.lock().unwrap();
            upsert_contact(&mut guard, alias.clone(), peer_id.to_string());
            save_contacts(&guard);
        }
        let endpoint = endpoint.clone();
        let ptt = ptt.clone();
        let ping_us = ping_us.clone();
        let mic_amp = mic_amp.clone();
        let audio_amp = audio_amp.clone();
        let input_device = args.input_device.clone();
        let output_device = args.output_device.clone();
        let chat_in = chat_in.clone();
        let chat_out_tx = chat_out_tx.clone();
        tokio::spawn(async move {
            let mut delay = Duration::from_secs(2);
            loop {
                match endpoint.connect(peer_id, iroh_roq::ALPN).await {
                    Ok(conn) => {
                        tokio::spawn(handle_conn(
                            conn, ptt, ping_us, mic_amp, audio_amp,
                            input_device, output_device,
                            chat_in, chat_out_tx, peer_alias,
                        ));
                        break;
                    }
                    Err(e) => {
                        tracing::warn!("connect failed ({e}), retrying in {}s…", delay.as_secs());
                        tokio::time::sleep(delay).await;
                        delay = (delay * 2).min(Duration::from_secs(30));
                    }
                }
            }
        });
    }

    tokio::select! {
        _ = &mut shutdown_rx => {}
        _ = tokio::signal::ctrl_c() => {}
    }

    running.store(false, Ordering::Relaxed);
    let _ = tui_thread.join();
    endpoint.close().await;
    Ok(())
}
