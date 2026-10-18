use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::settings::TranscribeSettings;
use crate::{models, paths};

/// What starts and stops a recording. Exactly one trigger is active.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Trigger {
    /// A key combination such as `"Alt+grave"` or `"Ctrl+Shift+space"`.
    Shortcut { keys: String },
    /// A single modifier key pressed on its own.
    ModifierKey { key: ModifierKey },
    /// An X11 mouse button: 2 = middle, 8/9 = back/forward thumb buttons.
    MouseButton { button: u8 },
    /// No global trigger; use `osw toggle` (e.g. bound in the desktop's
    /// keyboard settings).
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModifierKey {
    LeftCtrl,
    RightCtrl,
    LeftAlt,
    RightAlt,
    LeftShift,
    RightShift,
    LeftSuper,
    RightSuper,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Model filename in the models directory, or an absolute path. `None`
    /// picks [`models::default_model`].
    pub model: Option<String>,
    pub use_gpu: bool,
    pub transcription: TranscribeSettings,
    pub trigger: Trigger,
    /// Holding the trigger longer than 0.3 s records until release; a quick
    /// tap toggles.
    pub hold_to_record: bool,
    pub auto_paste: bool,
    /// Keep the transcription in the clipboard instead of restoring the
    /// previous clipboard contents after pasting.
    pub keep_in_clipboard: bool,
    pub add_space_after_sentence: bool,
    pub esc_cancel_without_confirmation: bool,
    /// PipeWire `node.name` of the microphone; `None` follows the default.
    pub microphone: Option<String>,
    /// Keep the microphone stream running between recordings so capture
    /// starts without device wake-up latency (the mic stays in use).
    pub keep_mic_warm: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: None,
            use_gpu: true,
            transcription: TranscribeSettings::default(),
            // A lone modifier is watched passively, so it cannot collide with
            // desktop shortcuts (Alt+grave, the macOS default, is Cinnamon's
            // and GNOME's "switch windows of an app").
            trigger: Trigger::ModifierKey { key: ModifierKey::RightCtrl },
            hold_to_record: true,
            auto_paste: true,
            keep_in_clipboard: false,
            add_space_after_sentence: true,
            esc_cancel_without_confirmation: false,
            microphone: None,
            keep_mic_warm: false,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("parsing {0}: {1}")]
    Parse(PathBuf, toml::de::Error),
}

impl Config {
    pub fn path() -> PathBuf {
        paths::config_dir().join("config.toml")
    }

    /// Loads the config, writing the defaults on first run so users have a
    /// file to edit.
    pub fn load_or_create() -> Result<Self, ConfigError> {
        let path = Self::path();
        if !path.exists() {
            let config = Self::default();
            if let Err(e) = config.save_to(&path) {
                log::warn!("could not write default config: {e}");
            }
            return Ok(config);
        }
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io(path.into(), e))?;
        toml::from_str(&text).map_err(|e| ConfigError::Parse(path.into(), e))
    }

    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self).expect("config serializes");
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(tmp, path)
    }

    /// Resolves the configured model to a file on disk.
    pub fn model_path(&self) -> Option<PathBuf> {
        match &self.model {
            Some(m) => {
                let direct = PathBuf::from(m);
                if direct.is_absolute() {
                    Some(direct)
                } else {
                    Some(paths::models_dir().join(m))
                }
            }
            None => models::default_model(),
        }
        .filter(|p| p.is_file())
    }

    /// Transcription settings with the model's required language applied
    /// (e.g. the Hebrew fine-tune).
    pub fn transcription_for(&self, model: &Path) -> TranscribeSettings {
        let mut settings = self.transcription.clone();
        let file = model.file_name().and_then(|f| f.to_str()).unwrap_or("");
        if let Some(lang) = models::preferred_language(file) {
            settings.language = lang.into();
        }
        settings
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_toml() {
        let config = Config {
            trigger: Trigger::ModifierKey { key: ModifierKey::RightCtrl },
            microphone: Some("alsa_input.usb-mic".into()),
            ..Default::default()
        };
        let text = toml::to_string_pretty(&config).unwrap();
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), config);
    }

    #[test]
    fn missing_fields_use_defaults() {
        let config: Config = toml::from_str(
            "auto_paste = false\n[trigger]\ntype = \"mouse_button\"\nbutton = 9\n",
        )
        .unwrap();
        assert!(!config.auto_paste);
        assert_eq!(config.trigger, Trigger::MouseButton { button: 9 });
        assert_eq!(config.transcription, TranscribeSettings::default());
    }
}
