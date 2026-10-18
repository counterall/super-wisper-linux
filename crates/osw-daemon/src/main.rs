//! OpenSuperWhisper desktop daemon: global trigger → record → transcribe →
//! paste. Every subsystem runs on its own thread and reports to the
//! controller loop below through one channel.

mod capture;
mod conflicts;
mod history;
mod input;
mod ipc;
mod keys;
mod overlay;
mod paste;
mod tray;
mod worker;

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel as cb;
use osw_core::config::Config;
use osw_core::engine::CancelToken;
use osw_core::history::{self as store, Entry, Status};
use osw_core::{text, EngineOptions, SAMPLE_RATE};

use capture::Capture;
use history::HistoryCmd;
use input::{Input, InputEvent};
use ipc::IpcCmd;
use overlay::{OverlayCmd, OverlayState};
use paste::PasteCmd;
use tray::{TrayCmd, TrayStatus};
use worker::{Audio, Job, WorkerCmd, WorkerEvent};

/// Holding the trigger at least this long means "record while held";
/// a shorter press toggles (matches the macOS app).
const HOLD_THRESHOLD: Duration = Duration::from_millis(300);
/// Window in which a second Esc confirms cancelling (matches the macOS app).
const CANCEL_CONFIRM_WINDOW: Duration = Duration::from_secs(5);
/// Shorter recordings are treated as accidental presses.
const MIN_RECORDING: Duration = Duration::from_millis(250);

enum Event {
    Input(InputEvent),
    Ipc(IpcCmd),
    Tray(TrayCmd),
    Worker(WorkerEvent),
}

/// How often old history entries are pruned at most.
const PRUNE_INTERVAL: Duration = Duration::from_secs(3600);

struct Recording {
    started_at: Instant,
    created_at_ms: i64,
    trigger_held: bool,
}

/// A dictation waiting for its text; shown by the overlay, cancellable.
struct Pending {
    id: u64,
    cancel: CancelToken,
    stopped_at: Instant,
    created_at_ms: i64,
    audio_secs: f32,
}

/// A file or history entry being (re)transcribed for the history window.
struct Background {
    entry_id: String,
    /// The audio must be stored when the job finishes (imported files).
    store_audio: bool,
}

struct App {
    config: Config,
    events: cb::Sender<Event>,
    capture: Capture,
    input: Option<Input>,
    overlay: cb::Sender<OverlayCmd>,
    paste: cb::Sender<PasteCmd>,
    worker: cb::Sender<WorkerCmd>,
    tray: Option<ksni::blocking::Handle<tray::Tray>>,
    recording: Option<Recording>,
    pending: VecDeque<Pending>,
    background: HashMap<u64, Background>,
    history: cb::Sender<HistoryCmd>,
    last_prune: Option<Instant>,
    confirm_cancel_until: Option<Instant>,
    next_id: u64,
    model_name: String,
    silence_warned_at: Option<Instant>,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,whisper_rs=warn")).init();
    let config = Config::load_or_create().context("loading config")?;
    log::info!("config: {}", Config::path().display());

    let (tx, rx) = cb::unbounded::<Event>();
    let capture = Capture::spawn(config.microphone.clone(), config.keep_mic_warm)
        .context("starting microphone capture (PipeWire)")?;
    {
        let tx = tx.clone();
        ipc::listen(capture.microphones_handle(), move |c| drop(tx.send(Event::Ipc(c))))?;
    }
    let history = history::spawn().context("opening the history database")?;
    let input = {
        let tx = tx.clone();
        match Input::spawn(&config.trigger, move |e| drop(tx.send(Event::Input(e)))) {
            Ok(input) => Some(input),
            Err(e) => {
                notify(&format!("Recording shortcut unavailable: {e:#}"));
                log::error!("trigger setup failed: {e:#}");
                None
            }
        }
    };
    let overlay = overlay::spawn(capture.level_handle())?;
    let paste = paste::spawn()?;
    let (worker, worker_thread) = {
        let tx = tx.clone();
        worker::spawn(move |e| drop(tx.send(Event::Worker(e))))?
    };
    let tray = {
        let tx = tx.clone();
        tray::Tray::spawn(config.microphone.clone(), capture.microphones_handle(), move |c| {
            drop(tx.send(Event::Tray(c)))
        })
    };

    let mut app = App {
        config,
        events: tx,
        capture,
        input,
        overlay,
        paste,
        worker,
        tray,
        recording: None,
        pending: VecDeque::new(),
        background: HashMap::new(),
        history,
        last_prune: None,
        confirm_cancel_until: None,
        next_id: 0,
        model_name: String::new(),
        silence_warned_at: None,
    };
    app.load_model();
    app.prune_history();
    let result = app.run(&rx);
    ipc::remove_socket();
    for p in &app.pending {
        p.cancel.cancel();
    }
    drop(app);
    let _ = worker_thread.join();
    result
}

impl App {
    fn run(&mut self, rx: &cb::Receiver<Event>) -> Result<()> {
        loop {
            let event = match self.confirm_cancel_until {
                Some(deadline) => match rx.recv_deadline(deadline) {
                    Ok(e) => e,
                    Err(cb::RecvTimeoutError::Timeout) => {
                        self.set_confirm_cancel(false);
                        continue;
                    }
                    Err(cb::RecvTimeoutError::Disconnected) => return Ok(()),
                },
                None => match rx.recv() {
                    Ok(e) => e,
                    Err(_) => return Ok(()),
                },
            };
            match event {
                Event::Input(InputEvent::TriggerDown) => self.trigger_down(),
                Event::Input(InputEvent::TriggerUp) => self.trigger_up(),
                Event::Input(InputEvent::Chord) => self.chord(),
                Event::Input(InputEvent::Escape) => self.escape(),
                Event::Ipc(cmd) => match cmd {
                    IpcCmd::Toggle => self.toggle(),
                    IpcCmd::Start if self.recording.is_none() => self.start_recording(false),
                    IpcCmd::Stop if self.recording.is_some() => self.stop_recording(),
                    IpcCmd::Start | IpcCmd::Stop => {}
                    IpcCmd::Cancel => self.cancel_all(),
                    IpcCmd::Reload => self.reload(),
                    IpcCmd::Quit => return Ok(()),
                    IpcCmd::TranscribeFile(path) => self.transcribe_file(path),
                    IpcCmd::Retranscribe(entry_id) => self.retranscribe(entry_id),
                },
                Event::Tray(cmd) => match cmd {
                    TrayCmd::Toggle => self.toggle(),
                    TrayCmd::SelectMicrophone(name) => {
                        self.config.microphone = name.clone();
                        self.capture.set_target(name);
                        self.save_config();
                    }
                    TrayCmd::OpenSettings => open_window(&["--settings"]),
                    TrayCmd::Quit => return Ok(()),
                },
                Event::Worker(WorkerEvent::Loaded(result)) => match result {
                    Ok(model) => {
                        self.model_name = display_name(&model);
                        self.update_tray();
                    }
                    Err(e) => {
                        log::error!("{e}");
                        notify(&format!("Could not load the Whisper model: {e}"));
                    }
                },
                Event::Worker(WorkerEvent::Progress { id, progress }) => {
                    if let Some(bg) = self.background.get(&id) {
                        let _ = self.history.send(HistoryCmd::SetStatus {
                            id: bg.entry_id.clone(),
                            status: Status::Transcribing,
                            progress,
                        });
                    }
                }
                Event::Worker(WorkerEvent::Done { id, samples, result }) => {
                    if let Some(bg) = self.background.remove(&id) {
                        self.background_done(bg, samples, result);
                    } else {
                        self.transcribed(id, samples, result);
                    }
                }
            }
        }
    }

    // --- trigger handling -------------------------------------------------

    fn trigger_down(&mut self) {
        if self.recording.is_some() {
            self.stop_recording();
        } else {
            self.start_recording(true);
        }
    }

    fn trigger_up(&mut self) {
        let Some(rec) = &mut self.recording else { return };
        let was_held = std::mem::replace(&mut rec.trigger_held, false);
        if was_held && self.config.hold_to_record && rec.started_at.elapsed() >= HOLD_THRESHOLD {
            self.stop_recording();
        }
    }

    /// The modifier-only trigger was part of a key combination (e.g. the
    /// user pressed Ctrl+C): drop the recording it started.
    fn chord(&mut self) {
        if self.recording.as_ref().is_some_and(|r| r.trigger_held) {
            log::debug!("trigger used in a key combination; discarding recording");
            self.capture.cancel();
            self.recording = None;
            self.refresh_indicators();
        }
    }

    fn toggle(&mut self) {
        if self.recording.is_some() {
            self.stop_recording();
        } else {
            self.start_recording(false);
        }
    }

    fn escape(&mut self) {
        if self.recording.is_none() && self.pending.is_empty() {
            return;
        }
        if self.config.esc_cancel_without_confirmation || self.confirm_cancel_until.is_some() {
            self.cancel_all();
        } else {
            self.set_confirm_cancel(true);
        }
    }

    fn set_confirm_cancel(&mut self, on: bool) {
        self.confirm_cancel_until = on.then(|| Instant::now() + CANCEL_CONFIRM_WINDOW);
        let _ = self.overlay.send(OverlayCmd::ConfirmCancel(on));
    }

    // --- recording lifecycle ----------------------------------------------

    fn start_recording(&mut self, trigger_held: bool) {
        self.capture.start();
        let _ = self.worker.send(WorkerCmd::Prepare);
        self.recording = Some(Recording {
            started_at: Instant::now(),
            created_at_ms: store::now_ms(),
            trigger_held,
        });
        self.refresh_indicators();
    }

    fn stop_recording(&mut self) {
        let Some(rec) = self.recording.take() else { return };
        let stopped_at = Instant::now();
        let recorded = match self.capture.stop().recv_timeout(Duration::from_secs(2)) {
            Ok(r) => r,
            Err(_) => {
                log::error!("capture did not return audio");
                self.refresh_indicators();
                return;
            }
        };
        let duration = rec.started_at.elapsed();
        log::info!(
            "recorded {:.2}s ({} samples, mic start latency {:?})",
            duration.as_secs_f32(),
            recorded.samples.len(),
            recorded.start_latency
        );
        if duration < MIN_RECORDING || recorded.samples.is_empty() {
            self.refresh_indicators();
            return;
        }
        // Digital silence means no microphone signal at all (unplugged mic,
        // default input pointing at a speaker monitor), not a quiet room.
        let peak = recorded.samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        if peak < 1e-4 {
            log::warn!("recording is silent (peak {peak:e})");
            if self.silence_warned_at.is_none_or(|t| t.elapsed() > Duration::from_secs(60)) {
                self.silence_warned_at = Some(Instant::now());
                notify(
                    "The microphone delivered no sound. Is a microphone connected? \
                     Choose one in the tray menu under Microphone.",
                );
            }
            self.refresh_indicators();
            return;
        }

        // Read the clipboard to restore while the model is busy.
        if self.config.auto_paste && !self.config.keep_in_clipboard {
            let _ = self.paste.send(PasteCmd::Snapshot);
        }
        let id = self.next_id;
        self.next_id += 1;
        let cancel = CancelToken::default();
        let audio_secs = recorded.samples.len() as f32 / SAMPLE_RATE as f32;
        let settings = match self.config.model_path() {
            Some(model) => self.config.transcription_for(&model),
            None => self.config.transcription.clone(),
        };
        let _ = self.worker.send(WorkerCmd::Transcribe(Job {
            id,
            audio: Audio::Samples(recorded.samples),
            settings,
            cancel: cancel.clone(),
            background: false,
        }));
        self.pending.push_back(Pending {
            id,
            cancel,
            stopped_at,
            created_at_ms: rec.created_at_ms,
            audio_secs,
        });
        self.refresh_indicators();
    }

    fn transcribed(&mut self, id: u64, samples: Vec<f32>, result: Result<osw_core::Transcription, String>) {
        let Some(pos) = self.pending.iter().position(|p| p.id == id) else { return };
        let job = self.pending.remove(pos).unwrap();
        self.refresh_indicators();
        if job.cancel.is_cancelled() {
            return;
        }
        match result {
            Ok(t) => {
                log::info!(
                    "transcribed {:.1}s audio ({:.1}s speech) in {:?} (vad {:?}, decode {:?}): {:?}",
                    job.audio_secs,
                    t.stats.speech_secs,
                    job.stopped_at.elapsed(),
                    t.stats.vad,
                    t.stats.decode,
                    t.text
                );
                if t.text.is_empty() {
                    return; // nothing was said: not worth keeping
                }
                let c = &self.config;
                if c.auto_paste || c.keep_in_clipboard {
                    let _ = self.paste.send(PasteCmd::Insert {
                        text: text::finalize_for_insert(&t.text, c.add_space_after_sentence),
                        paste: c.auto_paste,
                        keep_in_clipboard: c.keep_in_clipboard,
                    });
                }
                self.save_dictation(&job, samples, Ok(t.text));
            }
            Err(e) if e.contains("cancelled") => {}
            Err(e) => {
                log::error!("transcription failed: {e}");
                notify(&format!("Transcription failed: {e}"));
                // Keep the audio so the dictation can be re-transcribed.
                self.save_dictation(&job, samples, Err(e));
            }
        }
    }

    // --- history ----------------------------------------------------------

    fn save_dictation(&mut self, job: &Pending, samples: Vec<f32>, result: Result<String, String>) {
        if !self.config.save_history {
            return;
        }
        let entry_id = store::new_id();
        let (status, text, error) = match result {
            Ok(text) => (Status::Done, text, None),
            Err(e) => (Status::Failed, String::new(), Some(e)),
        };
        let _ = self.history.send(HistoryCmd::Insert(Entry {
            id: entry_id.clone(),
            created_at: job.created_at_ms,
            duration_secs: job.audio_secs,
            text,
            status,
            progress: if status == Status::Done { 1.0 } else { 0.0 },
            model: self.model_name.clone(),
            source_name: None,
            error,
        }));
        let _ = self.history.send(HistoryCmd::SaveAudio { id: entry_id, samples });
        self.prune_history();
    }

    fn prune_history(&mut self) {
        let Some(days) = self.config.history_retention_days else { return };
        if self.last_prune.is_some_and(|t| t.elapsed() < PRUNE_INTERVAL) {
            return;
        }
        self.last_prune = Some(Instant::now());
        let _ = self.history.send(HistoryCmd::Prune { days });
    }

    fn settings(&self) -> osw_core::TranscribeSettings {
        match self.config.model_path() {
            Some(model) => self.config.transcription_for(&model),
            None => self.config.transcription.clone(),
        }
    }

    fn queue_background(&mut self, entry_id: String, audio: Audio, store_audio: bool) {
        let id = self.next_id;
        self.next_id += 1;
        self.background.insert(id, Background { entry_id, store_audio });
        let _ = self.worker.send(WorkerCmd::Transcribe(Job {
            id,
            audio,
            settings: self.settings(),
            cancel: CancelToken::default(),
            background: true,
        }));
    }

    fn transcribe_file(&mut self, path: std::path::PathBuf) {
        let entry_id = store::new_id();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        let _ = self.history.send(HistoryCmd::Insert(Entry {
            id: entry_id.clone(),
            created_at: store::now_ms(),
            duration_secs: 0.0,
            text: String::new(),
            status: Status::Pending,
            progress: 0.0,
            model: self.model_name.clone(),
            source_name: name,
            error: None,
        }));
        self.queue_background(entry_id, Audio::File(path), true);
    }

    fn retranscribe(&mut self, entry_id: String) {
        let path = osw_core::paths::data_dir().join("recordings").join(format!("{entry_id}.wav"));
        let _ = self.history.send(HistoryCmd::SetStatus {
            id: entry_id.clone(),
            status: Status::Pending,
            progress: 0.0,
        });
        self.queue_background(entry_id, Audio::File(path), false);
    }

    fn background_done(&mut self, bg: Background, samples: Vec<f32>, result: Result<osw_core::Transcription, String>) {
        if bg.store_audio && !samples.is_empty() {
            let _ = self.history.send(HistoryCmd::SetDuration {
                id: bg.entry_id.clone(),
                secs: samples.len() as f32 / SAMPLE_RATE as f32,
            });
            let _ = self.history.send(HistoryCmd::SaveAudio { id: bg.entry_id.clone(), samples });
        }
        let result = result.map(|t| (t.text, self.model_name.clone()));
        if let Err(e) = &result {
            log::error!("background transcription failed: {e}");
        }
        let _ = self.history.send(HistoryCmd::SetResult { id: bg.entry_id, result });
    }

    fn cancel_all(&mut self) {
        if self.recording.take().is_some() {
            self.capture.cancel();
        }
        for p in self.pending.drain(..) {
            p.cancel.cancel();
        }
        self.set_confirm_cancel(false);
        self.refresh_indicators();
    }

    fn refresh_indicators(&mut self) {
        let (state, status) = if self.recording.is_some() {
            (OverlayState::Recording, TrayStatus::Recording)
        } else if !self.pending.is_empty() {
            (OverlayState::Transcribing, TrayStatus::Transcribing)
        } else {
            (OverlayState::Hidden, TrayStatus::Idle)
        };
        let busy = state != OverlayState::Hidden;
        if !busy {
            self.confirm_cancel_until = None;
        }
        if let Some(input) = &self.input {
            input.grab_escape(busy);
        }
        let _ = self.overlay.send(OverlayCmd::Set(state));
        if let Some(tray) = &self.tray {
            tray.update(|t| t.status = status);
        }
    }

    fn update_tray(&self) {
        if let Some(tray) = &self.tray {
            let model = self.model_name.clone();
            let mic = self.config.microphone.clone();
            tray.update(|t| {
                t.model = model;
                t.microphone = mic;
            });
        }
    }

    // --- configuration ----------------------------------------------------

    fn load_model(&mut self) {
        match self.config.model_path() {
            Some(model) => {
                let mut opts = EngineOptions::default();
                opts.use_gpu &= self.config.use_gpu;
                opts.flash_attn &= opts.use_gpu;
                log::info!("loading model {}", model.display());
                let _ = self.worker.send(WorkerCmd::Load { model, opts });
            }
            None => notify(&format!(
                "No Whisper model found in {}. Download one with `osw download`.",
                osw_core::paths::models_dir().display()
            )),
        }
    }

    fn reload(&mut self) {
        let new = match Config::load_from(&Config::path()) {
            Ok(c) => c,
            Err(e) => {
                notify(&format!("Settings not reloaded: {e}"));
                return;
            }
        };
        let old = std::mem::replace(&mut self.config, new);
        if old.trigger != self.config.trigger {
            match &self.input {
                Some(input) => {
                    if let Err(e) = input.set_trigger(&self.config.trigger) {
                        notify(&format!("Recording shortcut unavailable: {e:#}"));
                    }
                }
                None => {
                    let tx = self.events.clone();
                    match Input::spawn(&self.config.trigger, move |e| drop(tx.send(Event::Input(e)))) {
                        Ok(input) => self.input = Some(input),
                        Err(e) => notify(&format!("Recording shortcut unavailable: {e:#}")),
                    }
                }
            }
        }
        if old.microphone != self.config.microphone {
            self.capture.set_target(self.config.microphone.clone());
        }
        if old.model != self.config.model || old.use_gpu != self.config.use_gpu {
            self.load_model();
        }
        if old.keep_mic_warm != self.config.keep_mic_warm {
            log::warn!("keep_mic_warm takes effect after a restart");
        }
        self.update_tray();
        log::info!("settings reloaded");
    }

    fn save_config(&self) {
        if let Err(e) = self.config.save_to(&Config::path()) {
            log::error!("saving config failed: {e}");
        }
    }
}

fn display_name(model: &std::path::Path) -> String {
    let file = model.file_name().and_then(|f| f.to_str()).unwrap_or_default();
    osw_core::models::find(file).map_or_else(|| file.to_string(), |m| m.name.to_string())
}

/// Desktop notification for errors the user must see.
fn notify(message: &str) {
    log::warn!("{message}");
    let _ = std::process::Command::new("notify-send")
        .args(["--app-name=OpenSuperWhisper", "-i", "audio-input-microphone", "OpenSuperWhisper", message])
        .spawn();
}

/// Starts the history/settings window (a separate GTK process, so the
/// daemon itself stays small and GTK-free).
fn open_window(args: &[&str]) {
    let sibling = std::env::current_exe().ok().and_then(|exe| Some(exe.parent()?.join("osw-ui")));
    let program = sibling.filter(|p| p.is_file()).unwrap_or_else(|| "osw-ui".into());
    if let Err(e) = std::process::Command::new(&program).args(args).spawn() {
        notify(&format!("Could not open the window ({}): {e}", program.display()));
    }
}
