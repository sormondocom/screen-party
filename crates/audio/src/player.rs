//! Audio output via cpal.
//!
//! Mirrors the `CpalLoopbackCapturer` structure: the cpal stream (which is
//! `!Send` on Windows WASAPI) lives on a dedicated thread.  `CpalPlayer`
//! itself holds only a `Sender<Vec<f32>>` and is therefore `Send`.
//!
//! `prebuffer_samples` controls jitter absorption: the callback outputs silence
//! until at least that many samples have accumulated, then starts playing.
//! Set to 0 for immediate playback (no buffering).

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;

use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Data, OutputCallbackInfo, SampleFormat, StreamConfig,
};

use crate::capturer::AudioError;

// ── Public struct ─────────────────────────────────────────────────────────────

pub struct CpalPlayer {
    tx:    mpsc::Sender<Vec<f32>>,
    /// Dropping this closes the channel, signalling the player thread to stop.
    _stop: SyncSender<()>,
}

impl CpalPlayer {
    /// Open the default output device.
    ///
    /// `prebuffer_samples` — number of interleaved f32 samples to accumulate
    /// before starting playback.  Pass `0` for no initial buffering.
    /// A value derived from `SpeedStats::recommended_buffer_ms()` absorbs
    /// network jitter without causing a noticeable startup gap.
    pub fn new(
        sample_rate:       u32,
        channels:          u16,
        prebuffer_samples: usize,
    ) -> Result<Self, AudioError> {
        let host   = cpal::default_host();
        let device = host.default_output_device().ok_or(AudioError::NoOutputDevice)?;

        let supported = device
            .default_output_config()
            .map_err(|e| AudioError::Backend(e.to_string()))?;
        let sample_format = supported.sample_format();

        let config = StreamConfig {
            channels,
            sample_rate: cpal::SampleRate(sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        let (sample_tx, sample_rx) = mpsc::channel::<Vec<f32>>();
        let (stop_tx,   stop_rx)   = mpsc::sync_channel::<()>(1);

        thread::Builder::new()
            .name("audio-player".into())
            .spawn(move || {
                player_thread(config, sample_format, sample_rx, stop_rx, prebuffer_samples)
            })
            .map_err(|e| AudioError::Backend(e.to_string()))?;

        Ok(Self { tx: sample_tx, _stop: stop_tx })
    }

    /// Queue a batch of interleaved f32 samples for playback.
    pub fn push(&self, samples: Vec<f32>) {
        let _ = self.tx.send(samples);
    }
}

// ── Player thread ─────────────────────────────────────────────────────────────

fn player_thread(
    config:            StreamConfig,
    sample_format:     SampleFormat,
    rx:                Receiver<Vec<f32>>,
    stop_rx:           Receiver<()>,
    prebuffer_samples: usize,
) {
    let host   = cpal::default_host();
    let device = match host.default_output_device() {
        Some(d) => d,
        None    => { eprintln!("audio: no output device in player thread"); return; }
    };

    let mut acc:        VecDeque<f32> = VecDeque::new();
    let mut prebuffering = prebuffer_samples > 0;

    let err_fn = |e: cpal::StreamError| eprintln!("audio output error: {e}");

    let stream = device.build_output_stream_raw(
        &config,
        sample_format,
        move |data: &mut Data, _: &OutputCallbackInfo| {
            // Drain all newly arrived sample batches into the accumulator.
            while let Ok(chunk) = rx.try_recv() {
                acc.extend(chunk);
            }
            // Hold silence until the pre-buffer is satisfied.
            if prebuffering {
                if acc.len() < prebuffer_samples {
                    fill_silence(data);
                    return;
                }
                prebuffering = false;
            }
            fill_output(data, &mut acc);
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
            // Block until CpalPlayer is dropped (stop_tx closes).
            let _ = stop_rx.recv();
        }
        Err(e) => eprintln!("audio: build_output_stream_raw failed: {e}"),
    }
}

// ── Output helpers ────────────────────────────────────────────────────────────

/// Drain samples from `acc` into the device buffer, converting f32→native.
/// Any shortfall is zero-filled (silence).
fn fill_output(data: &mut Data, acc: &mut VecDeque<f32>) {
    match data.sample_format() {
        SampleFormat::F32 => {
            if let Some(s) = data.as_slice_mut::<f32>() {
                for out in s.iter_mut() {
                    *out = acc.pop_front().unwrap_or(0.0);
                }
            }
        }
        SampleFormat::I16 => {
            if let Some(s) = data.as_slice_mut::<i16>() {
                for out in s.iter_mut() {
                    *out = (acc.pop_front().unwrap_or(0.0) * i16::MAX as f32) as i16;
                }
            }
        }
        SampleFormat::U16 => {
            if let Some(s) = data.as_slice_mut::<u16>() {
                for out in s.iter_mut() {
                    *out = ((acc.pop_front().unwrap_or(0.0) + 1.0) * 32_768.0) as u16;
                }
            }
        }
        _ => {}
    }
}

/// Fill the device buffer with silence (format-appropriate zero).
fn fill_silence(data: &mut Data) {
    match data.sample_format() {
        SampleFormat::F32 => {
            if let Some(s) = data.as_slice_mut::<f32>()  { s.fill(0.0); }
        }
        SampleFormat::I16 => {
            if let Some(s) = data.as_slice_mut::<i16>()  { s.fill(0); }
        }
        SampleFormat::U16 => {
            if let Some(s) = data.as_slice_mut::<u16>()  { s.fill(32_768); }
        }
        _ => {}
    }
}
