//! Transcription worker: owns the loaded model and runs jobs in order.
//! Dictations always go before background jobs (files, re-transcriptions);
//! a background job that already started runs to completion.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel as cb;
use osw_core::engine::CancelToken;
use osw_core::{audio, EngineOptions, TranscribeSettings, Transcription, WhisperEngine};

pub enum Audio {
    Samples(Vec<f32>),
    /// Decoded on the worker thread (any supported format).
    File(PathBuf),
}

pub struct Job {
    pub id: u64,
    pub audio: Audio,
    pub settings: TranscribeSettings,
    pub cancel: CancelToken,
    pub background: bool,
}

pub enum WorkerCmd {
    Load { model: PathBuf, opts: EngineOptions },
    /// Pre-allocate decoding state; sent when a recording starts.
    Prepare,
    Transcribe(Job),
}

pub enum WorkerEvent {
    Loaded(Result<PathBuf, String>),
    Progress { id: u64, progress: f32 },
    Done {
        id: u64,
        /// The 16 kHz audio that was transcribed (empty if decoding failed).
        samples: Vec<f32>,
        result: Result<Transcription, String>,
    },
}

/// The worker exits, freeing the model, once the returned sender is
/// dropped; join the handle before the process exits so GPU buffers are
/// released while the CUDA driver is still alive.
pub fn spawn(
    on_event: impl Fn(WorkerEvent) + Send + 'static,
) -> Result<(cb::Sender<WorkerCmd>, std::thread::JoinHandle<()>)> {
    let (tx, rx) = cb::unbounded::<WorkerCmd>();
    let handle = std::thread::Builder::new()
        .name("whisper".into())
        .spawn(move || Worker { engine: None, on_event: Box::new(on_event) }.run(rx))?;
    Ok((tx, handle))
}

struct Worker {
    engine: Option<WhisperEngine>,
    on_event: Box<dyn Fn(WorkerEvent) + Send>,
}

impl Worker {
    fn run(&mut self, rx: cb::Receiver<WorkerCmd>) {
        let mut dictations = VecDeque::new();
        let mut background = VecDeque::new();
        loop {
            // Take everything queued so a waiting dictation is seen before
            // the next background job starts.
            let first = if dictations.is_empty() && background.is_empty() {
                match rx.recv() {
                    Ok(cmd) => Some(cmd),
                    Err(_) => return,
                }
            } else {
                None
            };
            for cmd in first.into_iter().chain(rx.try_iter()) {
                match cmd {
                    WorkerCmd::Load { model, opts } => self.load(model, &opts),
                    WorkerCmd::Prepare => {
                        if let Some(e) = &self.engine {
                            if let Err(err) = e.prepare() {
                                log::warn!("preparing state failed: {err}");
                            }
                        }
                    }
                    WorkerCmd::Transcribe(job) if job.background => background.push_back(job),
                    WorkerCmd::Transcribe(job) => dictations.push_back(job),
                }
            }
            if let Some(job) = dictations.pop_front().or_else(|| background.pop_front()) {
                self.transcribe(job);
            }
        }
    }

    fn load(&mut self, model: PathBuf, opts: &EngineOptions) {
        self.engine = None; // free VRAM before loading the next model
        let t = Instant::now();
        let result = WhisperEngine::load(&model, opts)
            .and_then(|e| e.warm_up().map(|_| e))
            .with_context(|| format!("loading {}", model.display()));
        match result {
            Ok(e) => {
                log::info!("model {} ready in {:?}", model.display(), t.elapsed());
                self.engine = Some(e);
                (self.on_event)(WorkerEvent::Loaded(Ok(model)));
            }
            Err(e) => (self.on_event)(WorkerEvent::Loaded(Err(format!("{e:#}")))),
        }
    }

    fn transcribe(&mut self, job: Job) {
        let samples = match job.audio {
            Audio::Samples(s) => s,
            Audio::File(path) => match audio::load_file(&path) {
                Ok(s) => s,
                Err(e) => {
                    let result = Err(format!("{}: {e}", path.display()));
                    (self.on_event)(WorkerEvent::Done { id: job.id, samples: Vec::new(), result });
                    return;
                }
            },
        };
        let Some(engine) = &self.engine else {
            let result = Err("no model loaded".into());
            (self.on_event)(WorkerEvent::Done { id: job.id, samples, result });
            return;
        };

        let on_event = &self.on_event;
        let mut last = Instant::now();
        let mut report = |p: f32| {
            if last.elapsed() >= Duration::from_millis(500) {
                last = Instant::now();
                on_event(WorkerEvent::Progress { id: job.id, progress: p });
            }
        };
        let progress: Option<&mut dyn FnMut(f32)> = if job.background { Some(&mut report) } else { None };
        let result = engine
            .transcribe(&samples, &job.settings, &job.cancel, progress)
            .map_err(|err| err.to_string());
        (self.on_event)(WorkerEvent::Done { id: job.id, samples, result });
    }
}
