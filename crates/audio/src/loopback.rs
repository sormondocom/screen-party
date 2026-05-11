//! System-audio loopback capture via cpal.
//!
//! `cpal::Stream` is `!Send` on some platforms (Windows WASAPI exposes this
//! through an internal marker type), so the entire cpal resource lifecycle
//! (host, device, stream) is confined to a dedicated audio thread.
//! `CpalLoopbackCapturer` holds only `Receiver<AudioFrame>` and a stop
//! signal — both `Send` — and therefore satisfies `AudioCapturer: Send`.
//!
//! Platform loopback strategy:
//!   Windows — default *output* device via WASAPI loopback (no virtual cable).
//!   macOS   — default *input* device (mic); true loopback needs BlackHole etc.
//!   Linux   — default *input* device; set it to a PipeWire/PA monitor in
//!             pavucontrol for system audio.

use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;
use std::time::Duration;

use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Data, InputCallbackInfo, SampleFormat, StreamConfig,
};

use crate::capturer::{AudioCapturer, AudioConfig, AudioError, AudioFrame};

// ── Public struct ─────────────────────────────────────────────────────────────

pub struct CpalLoopbackCapturer {
    receiver: Receiver<AudioFrame>,
    /// Dropping this closes the channel, signalling the audio thread to stop.
    _stop: SyncSender<()>,
    config: AudioConfig,
}

impl CpalLoopbackCapturer {
    pub fn new() -> Result<Self, AudioError> {
        // Query device config on this thread (no stream — just metadata).
        let (stream_config, sample_format) = probe_config()?;

        let channels = stream_config.channels;
        let sample_rate = stream_config.sample_rate.0;
        let config = AudioConfig { sample_rate, channels };

        let (frame_tx, frame_rx) = mpsc::channel::<AudioFrame>();
        let (stop_tx, stop_rx) = mpsc::sync_channel::<()>(1);

        // Clone plain-data items for the thread.
        let sc = stream_config.clone();
        let sf = sample_format;
        let cfg = config.clone();

        thread::spawn(move || audio_thread(sc, sf, cfg, frame_tx, stop_rx));

        Ok(Self {
            receiver: frame_rx,
            _stop: stop_tx,
            config,
        })
    }
}

impl AudioCapturer for CpalLoopbackCapturer {
    /// Block until a 20 ms frame of interleaved f32 samples is available.
    fn next_frame(&mut self) -> Result<AudioFrame, AudioError> {
        self.receiver
            .recv_timeout(Duration::from_secs(5))
            .map_err(|e| AudioError::Backend(e.to_string()))
    }

    fn config(&self) -> &AudioConfig {
        &self.config
    }
}

// ── Configuration probe ───────────────────────────────────────────────────────

fn probe_config() -> Result<(StreamConfig, SampleFormat), AudioError> {
    let host = cpal::default_host();

    let supported = loopback_supported_config(&host)?;
    let sample_format = supported.sample_format();
    let stream_config: StreamConfig = supported.into();
    Ok((stream_config, sample_format))
}

fn loopback_supported_config(
    host: &cpal::Host,
) -> Result<cpal::SupportedStreamConfig, AudioError> {
    #[cfg(target_os = "windows")]
    {
        let device = host
            .default_output_device()
            .ok_or(AudioError::NoLoopbackDevice)?;
        return device
            .default_output_config()
            .map_err(|e| AudioError::Backend(e.to_string()));
    }

    #[allow(unreachable_code)]
    {
        let device = host
            .default_input_device()
            .ok_or(AudioError::NoLoopbackDevice)?;
        device
            .default_input_config()
            .map_err(|e| AudioError::Backend(e.to_string()))
    }
}

// ── Audio thread ──────────────────────────────────────────────────────────────

fn audio_thread(
    stream_config: StreamConfig,
    sample_format: SampleFormat,
    audio_config: AudioConfig,
    tx: mpsc::Sender<AudioFrame>,
    stop_rx: mpsc::Receiver<()>,
) {
    let host = cpal::default_host();

    let device = {
        #[cfg(target_os = "windows")]
        {
            host.default_output_device()
        }
        #[cfg(not(target_os = "windows"))]
        {
            host.default_input_device()
        }
    };

    let device = match device {
        Some(d) => d,
        None => {
            eprintln!("audio: loopback device unavailable");
            return;
        }
    };

    // 20 ms frame in total samples (interleaved channels).
    let frame_len =
        (audio_config.sample_rate as usize / 50) * audio_config.channels as usize;

    let mut acc: Vec<f32> = Vec::with_capacity(frame_len * 4);

    let err_fn = |e: cpal::StreamError| eprintln!("audio stream error: {e}");

    // `build_input_stream_raw` gives us a `&Data` with the native byte layout;
    // we convert to f32 inside the callback to avoid three separate match arms
    // each trying to move `tx` and `acc`.
    let stream = device.build_input_stream_raw(
        &stream_config,
        sample_format,
        move |data: &Data, _: &InputCallbackInfo| {
            extend_f32(data, &mut acc);
            while acc.len() >= frame_len {
                let samples: Vec<f32> = acc.drain(..frame_len).collect();
                let _ = tx.send(AudioFrame {
                    samples,
                    channels: audio_config.channels,
                    sample_rate: audio_config.sample_rate,
                });
            }
        },
        err_fn,
        None,
    );

    match stream {
        Ok(s) => {
            if let Err(e) = s.play() {
                eprintln!("audio: play() failed: {e}");
                return;
            }
            // Block until the CpalLoopbackCapturer is dropped (stop_tx closes).
            let _ = stop_rx.recv();
            // `s` is dropped here, stopping the WASAPI capture session.
        }
        Err(e) => eprintln!("audio: build_input_stream_raw failed: {e}"),
    }
}

/// Convert a cpal `Data` buffer (any native format) into f32 and append to `out`.
fn extend_f32(data: &Data, out: &mut Vec<f32>) {
    match data.sample_format() {
        SampleFormat::F32 => {
            if let Some(s) = data.as_slice::<f32>() {
                out.extend_from_slice(s);
            }
        }
        SampleFormat::I16 => {
            if let Some(s) = data.as_slice::<i16>() {
                out.extend(s.iter().map(|&v| v as f32 / i16::MAX as f32));
            }
        }
        SampleFormat::U16 => {
            if let Some(s) = data.as_slice::<u16>() {
                out.extend(s.iter().map(|&v| (v as f32 - 32_768.0) / 32_768.0));
            }
        }
        _ => {} // unsupported format — skip silently
    }
}
