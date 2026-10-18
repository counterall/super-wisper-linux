//! The Rust VAD segmentation must match whisper.cpp's exactly.

use std::path::Path;

use osw_core::engine::Vad;
use osw_core::vad::{segments_from_probs, SpeechSegment};
use whisper_rs::{WhisperVadContext, WhisperVadContextParams, WhisperVadParams};

const JFK: &str = "../../../OpenSuperWhisper/jfk.wav";

fn load_jfk() -> Option<Vec<f32>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(JFK);
    path.is_file().then(|| osw_core::audio::load_file(&path).unwrap())
}

/// Speech, a long pause, then the same speech again.
fn with_pause(jfk: &[f32]) -> Vec<f32> {
    let mut s = jfk.to_vec();
    s.extend(std::iter::repeat_n(0.0, 16_000 * 2));
    s.extend_from_slice(jfk);
    s
}

fn reference(samples: &[f32]) -> Vec<SpeechSegment> {
    let path = osw_core::paths::vad_model_path().unwrap();
    let mut params = WhisperVadContextParams::default();
    params.set_n_threads(1);
    params.set_use_gpu(false);
    let mut ctx = WhisperVadContext::new(&path.to_string_lossy(), params).unwrap();
    ctx.segments_from_samples(WhisperVadParams::new(), samples)
        .unwrap()
        .map(|s| SpeechSegment { start_cs: s.start, end_cs: s.end })
        .collect()
}

#[test]
fn rust_segmentation_matches_whisper_cpp() {
    let Some(jfk) = load_jfk() else { return };
    let vad = Vad::load().unwrap();
    for samples in [jfk.clone(), with_pause(&jfk)] {
        let probs = vad.probs(&samples).unwrap();
        assert_eq!(segments_from_probs(&probs), reference(&samples));
    }
}
