use thiserror::Error;

#[derive(Debug, Error)]
pub enum AudioError {
    #[error("no loopback (what-you-hear) device found")]
    NoLoopbackDevice,
    #[error("no audio output device found")]
    NoOutputDevice,
    #[error("audio backend error: {0}")]
    Backend(String),
}

/// A chunk of interleaved PCM audio samples.
#[derive(Debug, Clone)]
pub struct AudioFrame {
    pub samples: Vec<f32>,
    pub channels: u16,
    pub sample_rate: u32,
}

#[derive(Debug, Clone)]
pub struct AudioConfig {
    pub sample_rate: u32,
    pub channels: u16,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self { sample_rate: 48_000, channels: 2 }
    }
}

/// Captures system audio output (loopback / "what you hear").
///
/// On Windows this uses WASAPI loopback mode — no virtual cable needed.
/// On macOS the user must grant Screen Recording permission and a virtual
/// audio device (e.g. BlackHole) may be required until macOS 14.2+.
/// On Linux we capture from the PulseAudio/PipeWire monitor source.
///
/// TODO: wire up cpal behind this trait; stub for now.
pub trait AudioCapturer: Send {
    fn next_frame(&mut self) -> Result<AudioFrame, AudioError>;
    fn config(&self) -> &AudioConfig;
}
