use std::fs::File;
use std::path::Path;

use rubato::{FftFixedIn, Resampler};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::{Error, Result, SAMPLE_RATE};

fn audio_err(e: impl std::fmt::Display) -> Error {
    Error::Audio(e.to_string())
}

/// Decodes any supported audio file (wav, mp3, flac, ogg/vorbis, m4a/aac/alac)
/// into 16 kHz mono f32. Channels are mixed down before resampling so the
/// resampler only processes one channel.
pub fn load_file(path: &Path) -> Result<Vec<f32>> {
    let file = File::open(path)?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(audio_err)?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| Error::Audio("no audio track".into()))?;
    let track_id = track.id;
    let source_rate = track
        .codec_params
        .sample_rate
        .ok_or_else(|| Error::Audio("unknown sample rate".into()))?;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(audio_err)?;

    let mut mono = match track.codec_params.n_frames {
        Some(n) => Vec::with_capacity(n as usize),
        None => Vec::new(),
    };
    let mut buf: Option<SampleBuffer<f32>> = None;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymphoniaError::ResetRequired) => break,
            Err(e) => return Err(audio_err(e)),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            // A corrupt packet should not discard the whole recording.
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => return Err(audio_err(e)),
        };
        let spec = *decoded.spec();
        let channels = spec.channels.count().max(1);
        let sb = match &mut buf {
            Some(sb) if sb.capacity() >= decoded.capacity() * channels => sb,
            _ => buf.insert(SampleBuffer::new(decoded.capacity() as u64, spec)),
        };
        sb.copy_interleaved_ref(decoded);
        mix_into(sb.samples(), channels, &mut mono);
    }

    if mono.is_empty() {
        return Err(Error::Audio("no audio samples decoded".into()));
    }
    resample_to_16k(&mono, source_rate)
}

fn mix_into(interleaved: &[f32], channels: usize, out: &mut Vec<f32>) {
    if channels == 1 {
        out.extend_from_slice(interleaved);
        return;
    }
    let scale = 1.0 / channels as f32;
    out.extend(
        interleaved
            .chunks_exact(channels)
            .map(|frame| frame.iter().sum::<f32>() * scale),
    );
}

/// Resamples mono audio to 16 kHz, compensating for the resampler delay and
/// flushing its tail so no audio is lost at either end.
pub fn resample_to_16k(input: &[f32], source_rate: u32) -> Result<Vec<f32>> {
    if source_rate == SAMPLE_RATE {
        return Ok(input.to_vec());
    }
    const CHUNK: usize = 4096;
    let mut resampler =
        FftFixedIn::<f32>::new(source_rate as usize, SAMPLE_RATE as usize, CHUNK, 2, 1)
            .map_err(audio_err)?;
    let expected =
        (input.len() as u64 * SAMPLE_RATE as u64).div_ceil(source_rate as u64) as usize;
    let delay = resampler.output_delay();
    let mut out = Vec::with_capacity(expected + delay + CHUNK);

    let mut pos = 0;
    while input.len() - pos >= resampler.input_frames_next() {
        let n = resampler.input_frames_next();
        let chunk = resampler
            .process(&[&input[pos..pos + n]], None)
            .map_err(audio_err)?;
        out.extend_from_slice(&chunk[0]);
        pos += n;
    }
    if pos < input.len() {
        let chunk = resampler
            .process_partial(Some(&[&input[pos..]][..]), None)
            .map_err(audio_err)?;
        out.extend_from_slice(&chunk[0]);
    }
    while out.len() < expected + delay {
        let chunk = resampler
            .process_partial::<&[f32]>(None, None)
            .map_err(audio_err)?;
        out.extend_from_slice(&chunk[0]);
    }

    out.drain(..delay.min(out.len()));
    out.truncate(expected);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, secs: f32, hz: f32) -> Vec<f32> {
        sine_shifted(rate, secs, hz, 0.0)
    }

    fn sine_shifted(rate: u32, secs: f32, hz: f32, shift: f32) -> Vec<f32> {
        let n = (rate as f32 * secs) as usize;
        (0..n)
            .map(|i| ((i as f32 + shift) / rate as f32 * hz * std::f32::consts::TAU).sin() * 0.5)
            .collect()
    }

    #[test]
    fn resample_preserves_duration_and_signal() {
        for rate in [44_100, 48_000, 22_050, 8_000] {
            let out = resample_to_16k(&sine(rate, 2.0, 440.0), rate).unwrap();
            assert_eq!(out.len(), 32_000, "rate {rate}");
            // Delay compensation is whole-sample, so allow a sub-sample shift.
            let best = (-5..=5)
                .map(|k| {
                    let reference = sine_shifted(SAMPLE_RATE, 2.0, 440.0, k as f32 * 0.1);
                    out[1000..31_000]
                        .iter()
                        .zip(&reference[1000..31_000])
                        .map(|(a, b)| (a - b).abs())
                        .fold(0.0, f32::max)
                })
                .fold(f32::MAX, f32::min);
            assert!(best < 0.01, "rate {rate}: max error {best}");
        }
    }

    #[test]
    fn stereo_is_averaged() {
        let mut out = Vec::new();
        mix_into(&[1.0, 0.0, 0.5, 0.5], 2, &mut out);
        assert_eq!(out, vec![0.5, 0.5]);
    }

    #[test]
    fn decodes_wav_files_at_any_rate() {
        let dir = std::env::temp_dir().join(format!("osw-audio-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stereo48k.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for s in sine(48_000, 1.0, 300.0) {
            let v = (s * i16::MAX as f32) as i16;
            w.write_sample(v).unwrap();
            w.write_sample(v).unwrap();
        }
        w.finalize().unwrap();

        let out = load_file(&path).unwrap();
        assert_eq!(out.len(), 16_000);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

