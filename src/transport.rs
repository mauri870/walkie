use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use iroh::endpoint::Connection;
use iroh_roq::{RtpPacket, Session, VarInt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::audio::{find_device, push_amp, rms_amplitude, start_capture, start_playback};
use crate::{AmpHistory, ChatBuffer, PlaybackBuf, FRAME_SIZE, MAX_LOG_LINES, SAMPLE_RATE};

const RTP_PAYLOAD_TYPE: u8 = 111;
const RTP_SSRC: u32 = 0x57414c4b;
const AUDIO_FLOW: VarInt = VarInt::from_u32(0);
const PING_INTERVAL: Duration = Duration::from_secs(2);

const STREAM_TYPE_CONTROL: u8 = 0x01;
const STREAM_TYPE_CHAT: u8 = 0x02;

enum Message {
    Ping,
    Pong,
    Chat(String),
}

impl Message {
    async fn write_to<W>(&self, writer: &mut W) -> Result<()>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        match self {
            Message::Ping => writer.write_all(&[0x01]).await?,
            Message::Pong => writer.write_all(&[0x02]).await?,
            Message::Chat(text) => {
                let bytes = text.as_bytes();
                let len = bytes.len() as u32;
                writer.write_all(&[0x03]).await?;
                writer.write_all(&len.to_be_bytes()).await?;
                writer.write_all(bytes).await?;
            }
        }
        Ok(())
    }

    async fn read_from<R>(reader: &mut R) -> Result<Self>
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        let mut tag = [0u8; 1];
        reader.read_exact(&mut tag).await?;
        match tag[0] {
            0x01 => Ok(Message::Ping),
            0x02 => Ok(Message::Pong),
            0x03 => {
                let mut len_buf = [0u8; 4];
                reader.read_exact(&mut len_buf).await?;
                let len = u32::from_be_bytes(len_buf) as usize;
                let mut text_buf = vec![0u8; len];
                reader.read_exact(&mut text_buf).await?;
                Ok(Message::Chat(String::from_utf8(text_buf)?))
            }
            other => anyhow::bail!("unknown message tag: {other:#x}"),
        }
    }
}

pub(crate) async fn handle_conn(
    conn: Connection,
    ptt: Arc<AtomicBool>,
    ping_us: Arc<AtomicU64>,
    mic_amp: AmpHistory,
    audio_amp: AmpHistory,
    input_device_name: Option<String>,
    output_device_name: Option<String>,
    chat_in: ChatBuffer,
    chat_out_tx: broadcast::Sender<String>,
    peer_alias: Option<String>,
) -> Result<()> {
    let peer = conn.remote_node_id()?.fmt_short();
    info!("connected to {peer}");

    // Accept loop — reads stream type byte and routes to control or chat handler
    let transport = conn.clone();
    let chat_in_accept = chat_in.clone();
    let peer_label = peer_alias.unwrap_or_else(|| peer.clone());
    tokio::spawn(async move {
        loop {
            match transport.accept_bi().await {
                Ok((mut send, mut recv)) => {
                    let chat_in = chat_in_accept.clone();
                    let peer_label = peer_label.clone();
                    tokio::spawn(async move {
                        let mut type_buf = [0u8; 1];
                        if recv.read_exact(&mut type_buf).await.is_err() {
                            return;
                        }
                        match type_buf[0] {
                            STREAM_TYPE_CONTROL => loop {
                                match Message::read_from(&mut recv).await {
                                    Ok(Message::Ping) => {
                                        if Message::Pong.write_to(&mut send).await.is_err() {
                                            break;
                                        }
                                    }
                                    _ => break,
                                }
                            },
                            STREAM_TYPE_CHAT => loop {
                                match Message::read_from(&mut recv).await {
                                    Ok(Message::Chat(text)) => {
                                        let mut buf = chat_in.lock().unwrap();
                                        if buf.len() >= MAX_LOG_LINES {
                                            buf.pop_front();
                                        }
                                        buf.push_back(format!("[{peer_label}] {text}"));
                                    }
                                    _ => break,
                                }
                            },
                            _ => {}
                        }
                    });
                }
                Err(_) => break,
            }
        }
    });

    // Ping sender — opens a control stream and measures round-trip time
    let transport = conn.clone();
    let ping_us_task = ping_us.clone();
    tokio::spawn(async move {
        let result: Result<()> = async {
            let (mut send, mut recv) = transport.open_bi().await?;
            send.write_all(&[STREAM_TYPE_CONTROL]).await?;
            let mut interval = tokio::time::interval(PING_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let t = std::time::Instant::now();
                Message::Ping.write_to(&mut send).await?;
                match Message::read_from(&mut recv).await? {
                    Message::Pong => {
                        let sample = t.elapsed().as_micros() as u64;
                        let old = ping_us_task.load(Ordering::Relaxed);
                        let ema = if old == 0 { sample } else { sample / 8 + old * 7 / 8 };
                        ping_us_task.store(ema, Ordering::Relaxed);
                    }
                    _ => break,
                }
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            warn!("ping: {e}");
        }
    });

    // Chat sender — opens a chat stream and forwards outgoing messages
    let transport = conn.clone();
    let mut chat_out_rx = chat_out_tx.subscribe();
    tokio::spawn(async move {
        let result: Result<()> = async {
            let (mut send, _recv) = transport.open_bi().await?;
            send.write_all(&[STREAM_TYPE_CHAT]).await?;
            loop {
                match chat_out_rx.recv().await {
                    Ok(text) => Message::Chat(text).write_to(&mut send).await?,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            warn!("chat: {e}");
        }
    });

    // Channels for audio — all Send, so handle_conn future stays Send.
    let (cap_tx, cap_rx) = tokio::sync::mpsc::channel::<Bytes>(64);
    let play_buf: PlaybackBuf = Arc::new(std::sync::Mutex::new(VecDeque::new()));

    // cpal::Stream is !Send; keep streams in a dedicated OS thread.
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    {
        let play_buf = play_buf.clone();
        let ptt = ptt.clone();
        let mic_amp = mic_amp.clone();
        std::thread::spawn(move || {
            let host = cpal::default_host();
            let in_dev = match find_device(&host, true, input_device_name.as_deref()) {
                Ok(d) => d,
                Err(e) => {
                    warn!("audio input: {e}");
                    return;
                }
            };
            let out_dev = match find_device(&host, false, output_device_name.as_deref()) {
                Ok(d) => d,
                Err(e) => {
                    warn!("audio output: {e}");
                    return;
                }
            };
            let _cap = match start_capture(&in_dev, ptt, mic_amp, cap_tx) {
                Ok(s) => s,
                Err(e) => {
                    warn!("capture start: {e}");
                    return;
                }
            };
            let _play = match start_playback(&out_dev, play_buf) {
                Ok(s) => s,
                Err(e) => {
                    warn!("playback start: {e}");
                    return;
                }
            };
            stop_rx.recv().ok();
        });
    }

    // RoQ session
    let session = Session::new(conn.clone());
    let send_flow = session.new_send_flow(AUDIO_FLOW).await?;
    let mut recv_flow = session.new_receive_flow(AUDIO_FLOW).await?;

    // Sender: read Opus frames from capture, wrap in RTP datagrams
    let send_handle = tokio::spawn(async move {
        let mut cap_rx = cap_rx;
        let mut seq: u16 = 0;
        let mut ts: u32 = 0;
        while let Some(payload) = cap_rx.recv().await {
            let pkt = RtpPacket {
                header: iroh_roq::rtp::header::Header {
                    version: 2,
                    payload_type: RTP_PAYLOAD_TYPE,
                    sequence_number: seq,
                    timestamp: ts,
                    ssrc: RTP_SSRC,
                    ..Default::default()
                },
                payload,
            };
            if send_flow.send_rtp(&pkt).is_err() {
                break;
            }
            seq = seq.wrapping_add(1);
            ts = ts.wrapping_add(FRAME_SIZE as u32);
        }
    });

    // Receiver: decode RTP to PCM and feed the playback buffer
    let recv_handle = tokio::spawn(async move {
        let mut decoder = match opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono) {
            Ok(d) => d,
            Err(e) => {
                warn!("opus decoder init: {e}");
                return;
            }
        };
        let mut pcm = vec![0f32; FRAME_SIZE];
        loop {
            match recv_flow.read_rtp().await {
                Ok(pkt) => {
                    if let Ok(n) = decoder.decode_float(&pkt.payload, &mut pcm, false) {
                        let samples = &pcm[..n];
                        push_amp(&audio_amp, rms_amplitude(samples));
                        play_buf.lock().unwrap().extend(samples.iter().copied());
                    }
                }
                Err(_) => break,
            }
        }
    });

    conn.closed().await;
    send_handle.abort();
    recv_handle.abort();
    stop_tx.send(()).ok();
    ping_us.store(0, Ordering::Relaxed);
    info!("disconnected from {peer}");
    Ok(())
}
