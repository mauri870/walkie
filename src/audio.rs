use std::sync::Arc;

use anyhow::Result;
use bytes::Bytes;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tracing::warn;

use crate::{AmpHistory, PlaybackBuf, AMP_HISTORY_LEN, FRAME_SIZE, SAMPLE_RATE};

pub(crate) fn rms_amplitude(samples: &[f32]) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    let rms = (samples.iter().map(|x| x * x).sum::<f32>() / samples.len() as f32).sqrt();
    (rms * 32_768.0) as u64
}

pub(crate) fn push_amp(history: &AmpHistory, amp: u64) {
    let mut h = history.lock().unwrap();
    if h.len() >= AMP_HISTORY_LEN {
        h.pop_front();
    }
    h.push_back(amp);
}

pub(crate) fn find_device(
    host: &cpal::Host,
    input: bool,
    name: Option<&str>,
) -> Result<cpal::Device> {
    match name {
        Some(n) => {
            let found = if input {
                host.input_devices()?.find(|d| d.name().map(|s| s == n).unwrap_or(false))
            } else {
                host.output_devices()?.find(|d| d.name().map(|s| s == n).unwrap_or(false))
            };
            found.ok_or_else(|| anyhow::anyhow!("device '{n}' not found"))
        }
        None => {
            if input {
                host.default_input_device()
                    .ok_or_else(|| anyhow::anyhow!("no default input device"))
            } else {
                host.default_output_device()
                    .ok_or_else(|| anyhow::anyhow!("no default output device"))
            }
        }
    }
}

fn find_input_config(device: &cpal::Device) -> Result<cpal::SupportedStreamConfigRange> {
    device
        .supported_input_configs()?
        .filter(|r| r.min_sample_rate().0 <= SAMPLE_RATE && r.max_sample_rate().0 >= SAMPLE_RATE)
        .min_by_key(|r| r.channels())
        .ok_or_else(|| anyhow::anyhow!("input device does not support {}Hz", SAMPLE_RATE))
}

fn find_output_config(device: &cpal::Device) -> Result<cpal::SupportedStreamConfigRange> {
    device
        .supported_output_configs()?
        .filter(|r| r.min_sample_rate().0 <= SAMPLE_RATE && r.max_sample_rate().0 >= SAMPLE_RATE)
        .min_by_key(|r| r.channels())
        .ok_or_else(|| anyhow::anyhow!("output device does not support {}Hz", SAMPLE_RATE))
}

fn build_capture_stream<S: cpal::SizedSample + 'static>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    to_f32: fn(S) -> f32,
    ptt: Arc<std::sync::atomic::AtomicBool>,
    mic_amp: AmpHistory,
    tx: tokio::sync::mpsc::Sender<Bytes>,
) -> Result<cpal::Stream> {
    let mut pcm_buf = Vec::<f32>::with_capacity(FRAME_SIZE * 4);
    let mut pcm_frame = vec![0f32; FRAME_SIZE];
    let mut encoder =
        opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip)?;
    let mut enc_buf = vec![0u8; 512];

    Ok(device.build_input_stream::<S, _, _>(
        config,
        move |data: &[S], _| {
            for chunk in data.chunks(channels) {
                let sum: f32 = chunk.iter().map(|s| to_f32(*s)).sum();
                pcm_buf.push(sum / channels as f32);
            }
            while pcm_buf.len() >= FRAME_SIZE {
                pcm_frame.copy_from_slice(&pcm_buf[..FRAME_SIZE]);
                pcm_buf.drain(..FRAME_SIZE);
                push_amp(&mic_amp, rms_amplitude(&pcm_frame));
                if ptt.load(std::sync::atomic::Ordering::Relaxed) {
                    if let Ok(n) = encoder.encode_float(&pcm_frame, &mut enc_buf) {
                        let _ = tx.try_send(Bytes::copy_from_slice(&enc_buf[..n]));
                    }
                }
            }
        },
        |err| warn!("capture error: {err}"),
        None,
    )?)
}

pub(crate) fn start_capture(
    device: &cpal::Device,
    ptt: Arc<std::sync::atomic::AtomicBool>,
    mic_amp: AmpHistory,
    tx: tokio::sync::mpsc::Sender<Bytes>,
) -> Result<cpal::Stream> {
    let range = find_input_config(device)?;
    let channels = range.channels() as usize;
    let fmt = range.sample_format();
    let config = range.with_sample_rate(cpal::SampleRate(SAMPLE_RATE)).config();

    let stream = match fmt {
        cpal::SampleFormat::F32 => {
            build_capture_stream::<f32>(device, &config, channels, |s| s, ptt, mic_amp, tx)
        }
        cpal::SampleFormat::I16 => build_capture_stream::<i16>(
            device,
            &config,
            channels,
            |s| s as f32 / 32_768.0,
            ptt,
            mic_amp,
            tx,
        ),
        cpal::SampleFormat::I32 => build_capture_stream::<i32>(
            device,
            &config,
            channels,
            |s| s as f32 / 2_147_483_648.0,
            ptt,
            mic_amp,
            tx,
        ),
        cpal::SampleFormat::U8 => build_capture_stream::<u8>(
            device,
            &config,
            channels,
            |s| (s as f32 - 128.0) / 128.0,
            ptt,
            mic_amp,
            tx,
        ),
        other => anyhow::bail!("unsupported input sample format: {other}"),
    }?;
    stream.play()?;
    Ok(stream)
}

fn build_playback_stream<S: cpal::SizedSample + 'static>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    from_f32: fn(f32) -> S,
    buf: PlaybackBuf,
) -> Result<cpal::Stream> {
    Ok(device.build_output_stream::<S, _, _>(
        config,
        move |data: &mut [S], _| {
            let mut b = buf.lock().unwrap();
            for frame in data.chunks_mut(channels) {
                let s = from_f32(b.pop_front().unwrap_or(0.0f32));
                for sample in frame.iter_mut() {
                    *sample = s;
                }
            }
        },
        |err| warn!("playback error: {err}"),
        None,
    )?)
}

pub(crate) fn start_playback(device: &cpal::Device, buf: PlaybackBuf) -> Result<cpal::Stream> {
    let range = find_output_config(device)?;
    let channels = range.channels() as usize;
    let fmt = range.sample_format();
    let config = range.with_sample_rate(cpal::SampleRate(SAMPLE_RATE)).config();

    let stream = match fmt {
        cpal::SampleFormat::F32 => {
            build_playback_stream::<f32>(device, &config, channels, |s| s, buf)
        }
        cpal::SampleFormat::I16 => build_playback_stream::<i16>(
            device,
            &config,
            channels,
            |s| (s * 32_767.0).clamp(i16::MIN as f32, i16::MAX as f32) as i16,
            buf,
        ),
        cpal::SampleFormat::I32 => build_playback_stream::<i32>(
            device,
            &config,
            channels,
            |s| (s * 2_147_483_647.0).clamp(i32::MIN as f32, i32::MAX as f32) as i32,
            buf,
        ),
        cpal::SampleFormat::U8 => build_playback_stream::<u8>(
            device,
            &config,
            channels,
            |s| ((s * 128.0) + 128.0).clamp(0.0, 255.0) as u8,
            buf,
        ),
        other => anyhow::bail!("unsupported output sample format: {other}"),
    }?;
    stream.play()?;
    Ok(stream)
}
