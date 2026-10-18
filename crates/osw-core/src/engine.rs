use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
    WhisperVadContext, WhisperVadContextParams, WhisperVadParams,
};

use crate::settings::TranscribeSettings;
use crate::text::{self, Segment};
use crate::vad::{self, SpeechSegment};
use crate::{paths, Error, Result, SAMPLE_RATE};

#[derive(Clone, Debug)]
pub struct EngineOptions {
    pub use_gpu: bool,
    pub gpu_device: i32,
    pub flash_attn: bool,
    pub n_threads: usize,
}

impl Default for EngineOptions {
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        let use_gpu = cfg!(any(feature = "vulkan", feature = "cuda"));
        Self {
            use_gpu,
            gpu_device: 0,
            // Faster on GPU; measured ~30% slower on CPU-only decoding.
            flash_attn: use_gpu,
            n_threads: cores.clamp(2, 8),
        }
    }
}

/// Shared cancellation flag, checked by whisper.cpp between compute steps.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub audio_secs: f32,
    pub speech_secs: f32,
    pub vad: Duration,
    pub state_init: Duration,
    pub decode: Duration,
}

#[derive(Clone, Debug, Default)]
pub struct Transcription {
    pub text: String,
    pub segments: Vec<Segment>,
    pub stats: Stats,
}

/// A loaded Whisper model plus the Silero VAD gate in front of it.
///
/// The model weights are shared; every transcription runs on its own fresh
/// `WhisperState`, so decoding context (prompt_past) never leaks between
/// recordings and a hallucination on silence cannot poison the next one.
pub struct WhisperEngine {
    ctx: WhisperContext,
    vad: Mutex<WhisperVadContext>,
    /// State created ahead of time (see [`Self::prepare`]) so its GPU buffer
    /// allocation is off the stop-to-text critical path.
    prepared: Mutex<Option<WhisperState>>,
    model_path: PathBuf,
    n_threads: usize,
}

impl WhisperEngine {
    pub fn load(model_path: &Path, opts: &EngineOptions) -> Result<Self> {
        whisper_rs::install_logging_hooks();

        let mut cparams = WhisperContextParameters::default();
        cparams
            .use_gpu(opts.use_gpu)
            .gpu_device(opts.gpu_device)
            .flash_attn(opts.flash_attn);
        let ctx = WhisperContext::new_with_params(model_path, cparams).map_err(|source| {
            Error::ModelLoad { path: model_path.display().to_string(), source }
        })?;

        let vad_path = paths::vad_model_path()?;
        let mut vparams = WhisperVadContextParams::default();
        vparams.set_n_threads(1);
        vparams.set_use_gpu(false);
        let vad = WhisperVadContext::new(&vad_path.to_string_lossy(), vparams).map_err(
            |source| Error::ModelLoad { path: vad_path.display().to_string(), source },
        )?;

        Ok(Self {
            ctx,
            vad: Mutex::new(vad),
            prepared: Mutex::new(None),
            model_path: model_path.to_path_buf(),
            n_threads: opts.n_threads,
        })
    }

    pub fn model_path(&self) -> &Path {
        &self.model_path
    }

    pub fn is_multilingual(&self) -> bool {
        self.ctx.is_multilingual()
    }

    /// Pre-allocates the decoding state for the next transcription. Call when
    /// a recording starts.
    pub fn prepare(&self) -> Result<()> {
        let mut prepared = self.prepared.lock().unwrap();
        if prepared.is_none() {
            *prepared = Some(self.ctx.create_state()?);
        }
        Ok(())
    }

    /// Runs one throwaway decode so GPU pipelines and kernels are compiled and
    /// buffers allocated before the first real dictation, then prepares state.
    pub fn warm_up(&self) -> Result<()> {
        let noise: Vec<f32> = (0..SAMPLE_RATE as usize)
            .map(|i| ((i * 7919) % 200) as f32 / 200.0 * 1e-3)
            .collect();
        let settings = TranscribeSettings::default();
        let mut state = self.ctx.create_state()?;
        self.decode(&mut state, &noise, &settings, &CancelToken::default(), None)?;
        self.vad
            .lock()
            .unwrap()
            .segments_from_samples(WhisperVadParams::new(), &noise)?;
        self.prepare()
    }

    /// Transcribes 16 kHz mono samples.
    pub fn transcribe(
        &self,
        samples: &[f32],
        settings: &TranscribeSettings,
        cancel: &CancelToken,
        progress: Option<&mut dyn FnMut(f32)>,
    ) -> Result<Transcription> {
        let mut stats = Stats {
            audio_secs: samples.len() as f32 / SAMPLE_RATE as f32,
            ..Default::default()
        };
        if samples.is_empty() {
            return Ok(Transcription { stats, ..Default::default() });
        }

        // VAD gate: whisper never sees non-speech audio, so silence cannot
        // produce hallucinated text and long pauses are not decoded at all.
        let t = Instant::now();
        let speech: Vec<SpeechSegment> = self
            .vad
            .lock()
            .unwrap()
            .segments_from_samples(WhisperVadParams::new(), samples)?
            .map(|s| SpeechSegment { start_cs: s.start, end_cs: s.end })
            .collect();
        stats.vad = t.elapsed();
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if speech.is_empty() {
            return Ok(Transcription { stats, ..Default::default() });
        }

        // Timestamps of trimmed audio would not match the original, so
        // trimming is applied only when timestamps are not requested.
        let trimmed;
        let input = if settings.show_timestamps {
            samples
        } else {
            trimmed = vad::speech_only_samples(samples, &speech);
            &trimmed
        };
        stats.speech_secs = input.len() as f32 / SAMPLE_RATE as f32;

        let t = Instant::now();
        let prepared = self.prepared.lock().unwrap().take();
        let mut state = match prepared {
            Some(state) => state,
            None => self.ctx.create_state()?,
        };
        stats.state_init = t.elapsed();

        let t = Instant::now();
        let segments = self.decode(&mut state, input, settings, cancel, progress)?;
        stats.decode = t.elapsed();

        Ok(Transcription { text: text::assemble(&segments, settings), segments, stats })
    }

    fn decode(
        &self,
        state: &mut WhisperState,
        samples: &[f32],
        settings: &TranscribeSettings,
        cancel: &CancelToken,
        progress: Option<&mut dyn FnMut(f32)>,
    ) -> Result<Vec<Segment>> {
        let mut params = self.full_params(settings);

        unsafe extern "C" fn abort_trampoline(user_data: *mut c_void) -> bool {
            unsafe { (*(user_data as *const AtomicBool)).load(Ordering::Relaxed) }
        }
        unsafe extern "C" fn progress_trampoline(
            _: *mut whisper_rs::WhisperSysContext,
            _: *mut whisper_rs::WhisperSysState,
            percent: std::ffi::c_int,
            user_data: *mut c_void,
        ) {
            let cb = unsafe { &mut *(user_data as *mut &mut dyn FnMut(f32)) };
            cb(percent as f32 / 100.0);
        }

        // Both pointers outlive `state.full`, which runs synchronously.
        let flag: &AtomicBool = &cancel.0;
        let mut progress = progress;
        unsafe {
            params.set_abort_callback(Some(abort_trampoline));
            params.set_abort_callback_user_data(flag as *const AtomicBool as *mut c_void);
            if let Some(cb) = progress.as_mut() {
                params.set_progress_callback(Some(progress_trampoline));
                params.set_progress_callback_user_data(
                    cb as *mut &mut dyn FnMut(f32) as *mut c_void,
                );
            }
        }

        let result = state.full(params, samples);
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        result?;

        Ok(state
            .as_iter()
            .map(|s| Segment {
                start_cs: s.start_timestamp(),
                end_cs: s.end_timestamp(),
                text: s.to_str_lossy().map(|t| t.into_owned()).unwrap_or_default(),
            })
            .collect())
    }

    fn full_params<'a>(&self, settings: &'a TranscribeSettings) -> FullParams<'a, 'a> {
        let strategy = if settings.beam_search {
            SamplingStrategy::BeamSearch { beam_size: settings.beam_size as i32, patience: -1.0 }
        } else {
            // Match whisper.cpp defaults: on temperature fallback the decoder
            // samples best_of candidates and keeps the most probable one.
            SamplingStrategy::Greedy { best_of: 5 }
        };
        let mut p = FullParams::new(strategy);
        p.set_n_threads(self.n_threads as i32);
        // prompt_past stays enabled between the 30 s windows inside one
        // recording; isolation between recordings comes from the fresh state.
        p.set_no_context(false);
        p.set_n_max_text_ctx((self.ctx.model_n_text_ctx() / 2).max(1));
        p.set_no_timestamps(!settings.show_timestamps);
        p.set_suppress_blank(settings.suppress_blank);
        p.set_language(Some(if settings.is_auto_language() { "auto" } else { &settings.language }));
        p.set_temperature(settings.temperature);
        p.set_no_speech_thold(settings.no_speech_threshold);
        if !settings.initial_prompt.is_empty() {
            p.set_initial_prompt(&settings.initial_prompt);
        }
        p.set_print_progress(false);
        p.set_print_realtime(false);
        p.set_print_special(false);
        p.set_print_timestamps(false);
        p
    }
}
