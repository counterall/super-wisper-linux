//! Core of OpenSuperWhisper for Linux: audio decoding, Silero VAD gating and
//! whisper.cpp transcription. No UI or desktop integration lives here.

pub mod audio;
pub mod engine;
pub mod models;
pub mod paths;
pub mod settings;
pub mod text;
pub mod vad;

pub use engine::{EngineOptions, Transcription, WhisperEngine};
pub use settings::TranscribeSettings;

/// Whisper's native input format: 16 kHz mono f32.
pub const SAMPLE_RATE: u32 = 16_000;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to load model {path}: {source}")]
    ModelLoad {
        path: String,
        source: whisper_rs::WhisperError,
    },
    #[error("whisper error: {0}")]
    Whisper(#[from] whisper_rs::WhisperError),
    #[error("audio error: {0}")]
    Audio(String),
    #[error("transcription cancelled")]
    Cancelled,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
