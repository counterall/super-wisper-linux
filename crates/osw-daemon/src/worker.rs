//! Transcription worker: owns the loaded model and runs jobs in order.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use crossbeam_channel as cb;
use osw_core::engine::CancelToken;
use osw_core::{EngineOptions, TranscribeSettings, Transcription, WhisperEngine};

pub struct Job {
    pub id: u64,
    pub samples: Vec<f32>,
    pub settings: TranscribeSettings,
    pub cancel: CancelToken,
}

pub enum WorkerCmd {
    Load { model: PathBuf, opts: EngineOptions },
    /// Pre-allocate decoding state; sent when a recording starts.
    Prepare,
    Transcribe(Job),
}

pub enum WorkerEvent {
    Loaded(Result<PathBuf, String>),
    Done { id: u64, result: Result<Transcription, String> },
}

/// The worker exits, freeing the model, once the returned sender is
/// dropped; join the handle before the process exits so GPU buffers are
/// released while the CUDA driver is still alive.
pub fn spawn(
    on_event: impl Fn(WorkerEvent) + Send + 'static,
) -> Result<(cb::Sender<WorkerCmd>, std::thread::JoinHandle<()>)> {
    let (tx, rx) = cb::unbounded::<WorkerCmd>();
    let handle = std::thread::Builder::new().name("whisper".into()).spawn(move || {
        let mut engine: Option<WhisperEngine> = None;
        for cmd in rx {
            match cmd {
                WorkerCmd::Load { model, opts } => {
                    engine = None; // free VRAM before loading the next model
                    let t = Instant::now();
                    let result = WhisperEngine::load(&model, &opts)
                        .and_then(|e| e.warm_up().map(|_| e))
                        .with_context(|| format!("loading {}", model.display()));
                    match result {
                        Ok(e) => {
                            log::info!("model {} ready in {:?}", model.display(), t.elapsed());
                            engine = Some(e);
                            on_event(WorkerEvent::Loaded(Ok(model)));
                        }
                        Err(e) => on_event(WorkerEvent::Loaded(Err(format!("{e:#}")))),
                    }
                }
                WorkerCmd::Prepare => {
                    if let Some(e) = &engine {
                        if let Err(err) = e.prepare() {
                            log::warn!("preparing state failed: {err}");
                        }
                    }
                }
                WorkerCmd::Transcribe(job) => {
                    let result = match &engine {
                        Some(e) => e
                            .transcribe(&job.samples, &job.settings, &job.cancel, None)
                            .map_err(|err| err.to_string()),
                        None => Err("no model loaded".into()),
                    };
                    on_event(WorkerEvent::Done { id: job.id, result });
                }
            }
        }
    })?;
    Ok((tx, handle))
}
