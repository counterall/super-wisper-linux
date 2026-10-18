//! Core of OpenSuperWhisper for Linux: audio decoding, Silero VAD gating and
//! whisper.cpp transcription. No UI or desktop integration lives here.

pub mod audio;
pub mod config;
#[cfg(feature = "engine")]
pub mod engine;
pub mod history;
pub mod models;
pub mod paths;
pub mod settings;
pub mod text;
pub mod vad;

#[cfg(feature = "engine")]
pub use engine::{EngineOptions, Transcription, WhisperEngine};
pub use settings::TranscribeSettings;

/// Whisper's native input format: 16 kHz mono f32.
pub const SAMPLE_RATE: u32 = 16_000;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[cfg(feature = "engine")]
    #[error("failed to load model {path}: {source}")]
    ModelLoad {
        path: String,
        source: whisper_rs::WhisperError,
    },
    #[cfg(feature = "engine")]
    #[error("whisper error: {0}")]
    Whisper(#[from] whisper_rs::WhisperError),
    #[error("audio error: {0}")]
    Audio(String),
    #[error("download failed: {0}")]
    Download(String),
    #[error("cancelled")]
    Cancelled,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
