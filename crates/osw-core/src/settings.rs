use serde::{Deserialize, Serialize};

/// Per-transcription decoding settings. Defaults match the macOS app.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TranscribeSettings {
    /// ISO 639-1 code, or "auto" for language detection.
    pub language: String,
    pub initial_prompt: String,
    pub temperature: f32,
    pub no_speech_threshold: f32,
    pub suppress_blank: bool,
    pub beam_search: bool,
    pub beam_size: u32,
    pub show_timestamps: bool,
    pub asian_autocorrect: bool,
}

impl Default for TranscribeSettings {
    fn default() -> Self {
        Self {
            language: "en".into(),
            initial_prompt: String::new(),
            temperature: 0.0,
            no_speech_threshold: 0.6,
            suppress_blank: true,
            beam_search: false,
            beam_size: 5,
            show_timestamps: false,
            asian_autocorrect: true,
        }
    }
}

pub const ASIAN_LANGUAGES: [&str; 3] = ["zh", "ja", "ko"];

impl TranscribeSettings {
    pub fn is_auto_language(&self) -> bool {
        self.language.is_empty() || self.language == "auto"
    }

    pub fn should_apply_asian_autocorrect(&self) -> bool {
        self.asian_autocorrect && ASIAN_LANGUAGES.contains(&self.language.as_str())
    }
}
