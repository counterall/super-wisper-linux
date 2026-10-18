/// A speech span reported by the Silero VAD, in centiseconds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpeechSegment {
    pub start_cs: f32,
    pub end_cs: f32,
}

const SAMPLES_PER_CS: usize = 160; // 16 kHz / 100
const OVERLAP_SAMPLES: usize = 1600; // 0.1 s
const GAP_SAMPLES: usize = 1600; // 0.1 s

/// Keeps only speech, mirroring upstream whisper_full VAD stitching: each
/// segment (already padded by the VAD) gets 0.1 s of the following audio as
/// overlap and segments are separated by 0.1 s of silence, so the decoder
/// still sees natural pauses between phrases.
pub fn speech_only_samples(samples: &[f32], segments: &[SpeechSegment]) -> Vec<f32> {
    let mut result = Vec::with_capacity(samples.len());
    for (index, segment) in segments.iter().enumerate() {
        let is_last = index + 1 == segments.len();
        let start = ((segment.start_cs.max(0.0) as usize) * SAMPLES_PER_CS).min(samples.len());
        let mut end = ((segment.end_cs.max(0.0) as usize) * SAMPLES_PER_CS).min(samples.len());
        if !is_last {
            end = (end + OVERLAP_SAMPLES).min(samples.len());
        }
        if end <= start {
            continue;
        }
        result.extend_from_slice(&samples[start..end]);
        if !is_last {
            result.resize(result.len() + GAP_SAMPLES, 0.0);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(start_cs: f32, end_cs: f32) -> SpeechSegment {
        SpeechSegment { start_cs, end_cs }
    }

    #[test]
    fn single_segment_is_cut_exactly() {
        let samples: Vec<f32> = (0..32_000).map(|i| i as f32).collect();
        let out = speech_only_samples(&samples, &[seg(50.0, 100.0)]);
        assert_eq!(out.len(), 8_000);
        assert_eq!(out[0], 8_000.0);
    }

    #[test]
    fn segments_get_overlap_and_gap() {
        let samples = vec![1.0; 64_000];
        let out = speech_only_samples(&samples, &[seg(0.0, 100.0), seg(200.0, 300.0)]);
        // first: 1 s + 0.1 s overlap, gap 0.1 s, second: 1 s
        assert_eq!(out.len(), 16_000 + 1_600 + 1_600 + 16_000);
        assert!(out[17_600..19_200].iter().all(|&s| s == 0.0));
    }

    #[test]
    fn out_of_range_segments_are_clamped_or_skipped() {
        let samples = vec![1.0; 16_000];
        let out = speech_only_samples(&samples, &[seg(50.0, 500.0), seg(600.0, 700.0)]);
        assert_eq!(out.len(), 8_000 + GAP_SAMPLES);
        assert!(speech_only_samples(&samples, &[]).is_empty());
    }
}
