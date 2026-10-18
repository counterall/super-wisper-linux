//! OpenSuperWhisper desktop daemon: global trigger → record → transcribe →
//! paste. Every subsystem runs on its own thread and reports to the
//! controller loop below through one channel.

mod capture;
mod input;
mod ipc;
mod keys;
mod overlay;
mod paste;
mod tray;
mod worker;

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel as cb;
use osw_core::config::Config;
use osw_core::engine::CancelToken;
use osw_core::{text, EngineOptions, SAMPLE_RATE};

use capture::Capture;
use input::{Input, InputEvent};
use ipc::IpcCmd;
use overlay::{OverlayCmd, OverlayState};
use paste::PasteCmd;
use tray::{TrayCmd, TrayStatus};
use worker::{Job, WorkerCmd, WorkerEvent};

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

struct Recording {
    started_at: Instant,
    trigger_held: bool,
}

struct Pending {
    id: u64,
    cancel: CancelToken,
    stopped_at: Instant,
    audio_secs: f32,
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
    confirm_cancel_until: Option<Instant>,
    next_id: u64,
    model_name: String,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,whisper_rs=warn")).init();
    let config = Config::load_or_create().context("loading config")?;
    log::info!("config: {}", Config::path().display());

    let (tx, rx) = cb::unbounded::<Event>();
    {
        let tx = tx.clone();
        ipc::listen(move |c| drop(tx.send(Event::Ipc(c))))?;
    }

    let capture = Capture::spawn(config.microphone.clone(), config.keep_mic_warm)
        .context("starting microphone capture (PipeWire)")?;
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
        confirm_cancel_until: None,
        next_id: 0,
        model_name: String::new(),
    };
    app.load_model();
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
                },
                Event::Tray(cmd) => match cmd {
                    TrayCmd::Toggle => self.toggle(),
                    TrayCmd::SelectMicrophone(name) => {
                        self.config.microphone = name.clone();
                        self.capture.set_target(name);
                        self.save_config();
                    }
                    TrayCmd::OpenConfig => {
                        let _ = std::process::Command::new("xdg-open").arg(Config::path()).spawn();
                    }
                    TrayCmd::Reload => self.reload(),
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
                Event::Worker(WorkerEvent::Done { id, result }) => self.transcribed(id, result),
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
        self.recording = Some(Recording { started_at: Instant::now(), trigger_held });
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
            samples: recorded.samples,
            settings,
            cancel: cancel.clone(),
        }));
        self.pending.push_back(Pending { id, cancel, stopped_at, audio_secs });
        self.refresh_indicators();
    }

    fn transcribed(&mut self, id: u64, result: Result<osw_core::Transcription, String>) {
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
                    return;
                }
                let c = &self.config;
                if c.auto_paste || c.keep_in_clipboard {
                    let _ = self.paste.send(PasteCmd::Insert {
                        text: text::finalize_for_insert(&t.text, c.add_space_after_sentence),
                        paste: c.auto_paste,
                        keep_in_clipboard: c.keep_in_clipboard,
                    });
                }
            }
            Err(e) if e.contains("cancelled") => {}
            Err(e) => {
                log::error!("transcription failed: {e}");
                notify(&format!("Transcription failed: {e}"));
            }
        }
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
