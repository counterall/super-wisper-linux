/// A speech span reported by the Silero VAD, in centiseconds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpeechSegment {
    pub start_cs: f32,
    pub end_cs: f32,
}

const SAMPLES_PER_CS: usize = 160; // 16 kHz / 100

/// Silero VAD window: one probability per 512 samples (32 ms).
pub const WINDOW: usize = 512;
/// Speech segments from per-window probabilities: a port of whisper.cpp's
/// `whisper_vad_segments_from_probs` with its default parameters
/// (threshold 0.5, min speech 250 ms, min silence 100 ms, pad 30 ms,
/// unlimited segment length).
pub fn segments_from_probs(probs: &[f32]) -> Vec<SpeechSegment> {
    const SR: usize = 16_000;
    let threshold = 0.5f32;
    let neg_threshold = (threshold - 0.15).max(0.01);
    let min_silence = SR * 100 / 1000;
    let min_speech = SR * 250 / 1000;
    let pad = SR * 30 / 1000;
    let audio_len = probs.len() * WINDOW;

    let mut speeches: Vec<(usize, usize)> = Vec::new();
    let mut in_speech = false;
    let mut temp_end = 0usize;
    let mut start = 0usize;

    for (i, &p) in probs.iter().enumerate() {
        let sample = i * WINDOW;
        if p >= threshold && temp_end != 0 {
            temp_end = 0;
        }
        if p >= threshold && !in_speech {
            in_speech = true;
            start = sample;
            continue;
        }
        if p < neg_threshold && in_speech {
            if temp_end == 0 {
                temp_end = sample;
            }
            if sample - temp_end < min_silence {
                continue;
            }
            if temp_end - start > min_speech {
                speeches.push((start, temp_end));
            }
            temp_end = 0;
            in_speech = false;
        }
    }
    if in_speech && audio_len - start > min_speech {
        speeches.push((start, audio_len));
    }

    // Merge segments separated by less than 200 ms.
    let max_gap = SR * 200 / 1000;
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(speeches.len());
    for seg in speeches {
        match merged.last_mut() {
            Some(last) if seg.0 - last.1 < max_gap => last.1 = seg.1,
            _ => merged.push(seg),
        }
    }
    merged.retain(|(s, e)| e - s >= min_speech);

    // Padding: split short gaps between neighbours, pad the rest.
    let n = merged.len();
    for i in 0..n {
        if i == 0 {
            merged[i].0 = merged[i].0.saturating_sub(pad);
        }
        if i + 1 < n {
            let silence = merged[i + 1].0 - merged[i].1;
            if silence < 2 * pad {
                merged[i].1 += silence / 2;
                merged[i + 1].0 = merged[i + 1].0.saturating_sub(silence / 2);
            } else {
                merged[i].1 = (merged[i].1 + pad).min(audio_len);
                merged[i + 1].0 = merged[i + 1].0.saturating_sub(pad);
            }
        } else {
            merged[i].1 = (merged[i].1 + pad).min(audio_len);
        }
    }

    let to_cs = |s: usize| ((s as f64 / SR as f64) * 100.0 + 0.5).floor() as f32;
    merged
        .into_iter()
        .map(|(s, e)| SpeechSegment { start_cs: to_cs(s), end_cs: to_cs(e) })
        .collect()
}

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

    #[test]
    fn segments_from_probs_basic_shapes() {
        let mut probs = vec![0.0f32; 100];
        // 1.6 s of speech starting at window 20.
        probs[20..70].iter_mut().for_each(|p| *p = 0.9);
        let segs = segments_from_probs(&probs);
        assert_eq!(segs.len(), 1);
        // start 20*512 = 10240 samples - 480 pad = 9760 -> 61 cs
        assert_eq!(segs[0].start_cs, 61.0);
        // end: silence starts at window 70 = 35840 + 480 pad = 36320 -> 227 cs
        assert_eq!(segs[0].end_cs, 227.0);
        // Too short to count as speech.
        let mut blip = vec![0.0f32; 100];
        blip[10..14].iter_mut().for_each(|p| *p = 0.9);
        assert!(segments_from_probs(&blip).is_empty());
    }

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
