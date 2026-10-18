use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::{paths, Error, Result};

/// A downloadable ggml Whisper model.
#[derive(Clone, Debug)]
pub struct CatalogModel {
    pub name: &'static str,
    pub filename: &'static str,
    pub url: &'static str,
    pub size_mb: u32,
    pub description: &'static str,
    /// Fine-tunes that require an explicit language.
    pub preferred_language: Option<&'static str>,
}

macro_rules! ggml {
    ($name:literal, $file:literal, $size:literal, $desc:literal) => {
        CatalogModel {
            name: $name,
            filename: $file,
            url: concat!("https://huggingface.co/ggerganov/whisper.cpp/resolve/main/", $file),
            size_mb: $size,
            description: $desc,
            preferred_language: None,
        }
    };
}

pub const CATALOG: &[CatalogModel] = &[
    ggml!("Turbo V3 large", "ggml-large-v3-turbo.bin", 1624, "High accuracy, best quality"),
    ggml!("Turbo V3 medium", "ggml-large-v3-turbo-q8_0.bin", 874, "Balanced speed and accuracy"),
    ggml!("Turbo V3 small", "ggml-large-v3-turbo-q5_0.bin", 574, "Fastest Turbo; recommended on GPU"),
    ggml!("Medium", "ggml-medium-q5_0.bin", 539, "Multilingual, a little faster than Turbo on GPU"),
    ggml!("Small", "ggml-small-q8_0.bin", 264, "Multilingual, good for CPU-only machines"),
    ggml!("Base", "ggml-base-q8_0.bin", 82, "Multilingual, very fast on CPU, lower accuracy"),
    ggml!("Tiny (English)", "ggml-tiny.en.bin", 78, "English only, smallest and fastest"),
    CatalogModel {
        name: "Turbo V3 Hebrew",
        filename: "ggml-ivrit-large-v3-turbo.bin",
        url: "https://huggingface.co/ivrit-ai/whisper-large-v3-turbo-ggml/resolve/main/ggml-model.bin",
        size_mb: 1624,
        description: "Hebrew fine-tune of Turbo V3 by ivrit.ai. Sets the language to Hebrew.",
        preferred_language: Some("he"),
    },
];

/// Preferred models when none is selected, best first: Turbo q5_0 is ~0.55 s
/// per dictation on a GTX 1070 (CUDA), Small q8_0 ~0.2 s with lower accuracy.
pub const DEFAULT_PREFERENCE: &[&str] = &["ggml-large-v3-turbo-q5_0.bin", "ggml-small-q8_0.bin"];

/// The model to use when the user has not picked one.
pub fn default_model() -> Option<PathBuf> {
    let installed = installed();
    DEFAULT_PREFERENCE
        .iter()
        .find_map(|name| installed.iter().find(|p| p.file_name().is_some_and(|f| f == *name)))
        .or(installed.first())
        .cloned()
}

pub fn find(filename: &str) -> Option<&'static CatalogModel> {
    CATALOG.iter().find(|m| m.filename == filename)
}

pub fn preferred_language(filename: &str) -> Option<&'static str> {
    find(filename).and_then(|m| m.preferred_language)
}

/// Installed `.bin` models, sorted by filename.
pub fn installed() -> Vec<PathBuf> {
    let mut models: Vec<PathBuf> = std::fs::read_dir(paths::models_dir())
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "bin"))
        .collect();
    models.sort();
    models
}

/// Downloads a catalog model into the models directory. `progress` gets
/// (bytes so far, total if known). The file only appears under its final
/// name once complete, so a cancelled or failed download leaves no model.
pub fn download(
    model: &CatalogModel,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<PathBuf> {
    let dir = paths::models_dir();
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join(model.filename);
    if dest.is_file() {
        return Ok(dest);
    }
    let part = dir.join(format!("{}.part", model.filename));

    let response = ureq::get(model.url)
        .call()
        .map_err(|e| Error::Download(e.to_string()))?;
    let total = response.body().content_length();
    let mut reader = response.into_body().into_reader();
    let mut file = std::io::BufWriter::new(std::fs::File::create(&part)?);
    let mut buf = vec![0u8; 1 << 16];
    let mut done = 0u64;
    let result = loop {
        if cancel.load(Ordering::Relaxed) {
            break Err(Error::Cancelled);
        }
        match reader.read(&mut buf) {
            Ok(0) => break Ok(()),
            Ok(n) => {
                file.write_all(&buf[..n])?;
                done += n as u64;
                progress(done, total);
            }
            Err(e) => break Err(Error::Download(e.to_string())),
        }
    };
    let result = result.and_then(|_| match total {
        Some(t) if t != done => Err(Error::Download(format!("incomplete: {done} of {t} bytes"))),
        _ => Ok(()),
    });
    if let Err(e) = result {
        drop(file);
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    file.flush()?;
    drop(file);
    std::fs::rename(&part, &dest)?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HF: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/";

    #[test]
    fn catalog_urls_point_at_their_files() {
        for m in CATALOG.iter().filter(|m| m.preferred_language.is_none()) {
            assert_eq!(m.url, format!("{HF}{}", m.filename));
        }
        assert_eq!(preferred_language("ggml-ivrit-large-v3-turbo.bin"), Some("he"));
        assert_eq!(preferred_language("ggml-large-v3-turbo.bin"), None);
    }
}
