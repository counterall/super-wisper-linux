use std::fs;
use std::path::PathBuf;

use directories::ProjectDirs;

/// Silero VAD model, embedded so the app ships as a single binary.
const VAD_MODEL: &[u8] = include_bytes!("../assets/ggml-silero-v5.1.2.bin");
const VAD_MODEL_FILENAME: &str = "ggml-silero-v5.1.2.bin";

fn project_dirs() -> ProjectDirs {
    ProjectDirs::from("", "", "opensuperwhisper").expect("no home directory")
}

/// `~/.local/share/opensuperwhisper`
pub fn data_dir() -> PathBuf {
    project_dirs().data_dir().to_path_buf()
}

/// `~/.config/opensuperwhisper`
pub fn config_dir() -> PathBuf {
    project_dirs().config_dir().to_path_buf()
}

/// Control socket of the running daemon (`$OSW_SOCKET` overrides it, e.g.
/// to run a second, isolated daemon for testing).
pub fn socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os("OSW_SOCKET") {
        return path.into();
    }
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    dir.join("opensuperwhisper.sock")
}

pub fn models_dir() -> PathBuf {
    data_dir().join("models")
}

/// Path of the VAD model on disk, extracting the embedded copy when missing
/// or stale (whisper.cpp only loads VAD models from files).
pub fn vad_model_path() -> std::io::Result<PathBuf> {
    let dir = data_dir();
    let path = dir.join(VAD_MODEL_FILENAME);
    let up_to_date = fs::metadata(&path)
        .map(|m| m.len() == VAD_MODEL.len() as u64)
        .unwrap_or(false);
    if !up_to_date {
        fs::create_dir_all(&dir)?;
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, VAD_MODEL)?;
        fs::rename(&tmp, &path)?;
    }
    Ok(path)
}
