//! Microphone capture through PipeWire.
//!
//! The stream asks for 16 kHz mono f32, so PipeWire's adapter does channel
//! mixing and resampling and samples land directly in Whisper's format. The
//! stream stays connected between recordings and is only (de)activated, which
//! keeps start latency to a node wake-up instead of a full stream setup.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use crossbeam_channel as cb;
use pipewire as pw;
use pw::spa;
use pw::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pw::spa::pod::Pod;

use osw_core::SAMPLE_RATE;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Microphone {
    pub id: u32,
    /// `node.name`, stable across restarts; used as the stream target.
    pub name: String,
    pub description: String,
}

pub struct Recorded {
    pub samples: Vec<f32>,
    /// Time from start request to the first captured buffer.
    pub start_latency: Option<Duration>,
}

enum Cmd {
    Start,
    Stop(cb::Sender<Recorded>),
    Cancel,
    SetTarget(Option<String>),
    Quit,
}

/// Handle to the capture thread.
pub struct Capture {
    tx: pw::channel::Sender<Cmd>,
    level: Arc<AtomicU32>,
    microphones: Arc<Mutex<Vec<Microphone>>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Capture {
    pub fn spawn(target: Option<String>, keep_warm: bool) -> Result<Self> {
        let (tx, rx) = pw::channel::channel();
        let level = Arc::new(AtomicU32::new(0));
        let microphones = Arc::new(Mutex::new(Vec::new()));
        let (ready_tx, ready_rx) = cb::bounded(1);

        let thread = {
            let level = level.clone();
            let microphones = microphones.clone();
            thread::Builder::new().name("pw-capture".into()).spawn(move || {
                if let Err(e) = run(rx, target, keep_warm, level, microphones, &ready_tx) {
                    let _ = ready_tx.send(Err(e));
                }
            })?
        };
        ready_rx.recv().map_err(|_| anyhow!("capture thread exited"))??;
        Ok(Self { tx, level, microphones, thread: Some(thread) })
    }

    pub fn start(&self) {
        let _ = self.tx.send(Cmd::Start);
    }

    /// Stops recording; the samples arrive on the returned channel.
    pub fn stop(&self) -> cb::Receiver<Recorded> {
        let (tx, rx) = cb::bounded(1);
        let _ = self.tx.send(Cmd::Stop(tx));
        rx
    }

    pub fn cancel(&self) {
        let _ = self.tx.send(Cmd::Cancel);
    }

    pub fn set_target(&self, target: Option<String>) {
        let _ = self.tx.send(Cmd::SetTarget(target));
    }

    /// RMS level of the latest buffer (f32 bits), for the overlay.
    pub fn level_handle(&self) -> Arc<AtomicU32> {
        self.level.clone()
    }

    pub fn microphones_handle(&self) -> Arc<Mutex<Vec<Microphone>>> {
        self.microphones.clone()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[derive(Default)]
struct State {
    recording: bool,
    samples: Vec<f32>,
    requested_at: Option<Instant>,
    start_latency: Option<Duration>,
    rate: u32,
    channels: u32,
}

struct ActiveStream {
    stream: pw::stream::StreamRc,
    _listener: pw::stream::StreamListener<()>,
}

fn run(
    rx: pw::channel::Receiver<Cmd>,
    target: Option<String>,
    keep_warm: bool,
    level: Arc<AtomicU32>,
    microphones: Arc<Mutex<Vec<Microphone>>>,
    ready: &cb::Sender<Result<()>>,
) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;

    let registry = core.get_registry_rc()?;
    let _registry_listener = {
        let added = microphones.clone();
        let removed = microphones.clone();
        registry
            .add_listener_local()
            .global(move |global| {
                if global.type_ != pw::types::ObjectType::Node {
                    return;
                }
                let Some(props) = global.props else { return };
                if props.get("media.class") != Some("Audio/Source") {
                    return;
                }
                let name = props.get("node.name").unwrap_or_default().to_string();
                let description = props
                    .get("node.description")
                    .or(props.get("node.nick"))
                    .unwrap_or(&name)
                    .to_string();
                let mut mics = added.lock().unwrap();
                mics.retain(|m| m.id != global.id);
                mics.push(Microphone { id: global.id, name, description });
            })
            .global_remove(move |id| removed.lock().unwrap().retain(|m| m.id != id))
            .register()
    };

    let state = Rc::new(RefCell::new(State::default()));
    let active: Rc<RefCell<Option<ActiveStream>>> = Rc::new(RefCell::new(None));
    *active.borrow_mut() = Some(build_stream(&core, target.as_deref(), keep_warm, &state, &level)?);
    let _ = ready.send(Ok(()));

    let _receiver = rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        let state = state.clone();
        let active = active.clone();
        let core = core.clone();
        let level = level.clone();
        move |cmd| {
            let set_active = |on: bool| {
                if keep_warm {
                    return;
                }
                if let Some(a) = active.borrow().as_ref() {
                    if let Err(e) = a.stream.set_active(on) {
                        log::error!("set_active({on}) failed: {e}");
                    }
                }
            };
            match cmd {
                Cmd::Start => {
                    {
                        let mut s = state.borrow_mut();
                        s.samples.clear();
                        s.samples.reserve(SAMPLE_RATE as usize * 30);
                        s.recording = true;
                        s.requested_at = Some(Instant::now());
                        s.start_latency = None;
                    }
                    set_active(true);
                }
                Cmd::Stop(reply) => {
                    set_active(false);
                    let mut s = state.borrow_mut();
                    s.recording = false;
                    let samples = std::mem::take(&mut s.samples);
                    let samples = if s.rate != 0 && s.rate != SAMPLE_RATE {
                        // The adapter should resample for us; this is a fallback.
                        osw_core::audio::resample_to_16k(&samples, s.rate).unwrap_or(samples)
                    } else {
                        samples
                    };
                    let _ = reply.send(Recorded { samples, start_latency: s.start_latency });
                    level.store(0, Ordering::Relaxed);
                }
                Cmd::Cancel => {
                    set_active(false);
                    let mut s = state.borrow_mut();
                    s.recording = false;
                    s.samples = Vec::new();
                    level.store(0, Ordering::Relaxed);
                }
                Cmd::SetTarget(target) => {
                    // Drop the old stream before connecting the new one.
                    active.borrow_mut().take();
                    match build_stream(&core, target.as_deref(), keep_warm, &state, &level) {
                        Ok(stream) => {
                            if state.borrow().recording && !keep_warm {
                                let _ = stream.stream.set_active(true);
                            }
                            *active.borrow_mut() = Some(stream);
                        }
                        Err(e) => log::error!("reconnecting microphone failed: {e}"),
                    }
                }
                Cmd::Quit => mainloop.quit(),
            }
        }
    });

    mainloop.run();
    Ok(())
}

fn build_stream(
    core: &pw::core::CoreRc,
    target: Option<&str>,
    keep_warm: bool,
    state: &Rc<RefCell<State>>,
    level: &Arc<AtomicU32>,
) -> Result<ActiveStream> {
    let mut props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Communication",
        *pw::keys::APP_NAME => "OpenSuperWhisper",
        *pw::keys::NODE_NAME => "opensuperwhisper",
    };
    if let Some(target) = target {
        props.insert(*pw::keys::TARGET_OBJECT, target);
    }
    let stream = pw::stream::StreamRc::new(core.clone(), "opensuperwhisper-capture", props)?;

    let listener = stream
        .add_local_listener_with_user_data(())
        .param_changed({
            let state = state.clone();
            move |_, _, id, param| {
                let Some(param) = param else { return };
                if id != spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let mut info = AudioInfoRaw::new();
                if info.parse(param).is_ok() {
                    let mut s = state.borrow_mut();
                    s.rate = info.rate();
                    s.channels = info.channels().max(1);
                    log::info!("capture format: {} Hz, {} ch", s.rate, s.channels);
                }
            }
        })
        .process({
            let state = state.clone();
            let level = level.clone();
            move |stream, _| {
                let Some(mut buffer) = stream.dequeue_buffer() else { return };
                let datas = buffer.datas_mut();
                let Some(data) = datas.first_mut() else { return };
                let size = data.chunk().size() as usize;
                let Some(bytes) = data.data() else { return };
                let bytes = &bytes[..size.min(bytes.len())];

                let mut s = state.borrow_mut();
                if !s.recording {
                    return;
                }
                if s.start_latency.is_none() {
                    s.start_latency = s.requested_at.map(|t| t.elapsed());
                }
                let channels = s.channels.max(1) as usize;
                let mut sum_sq = 0.0f32;
                let mut n = 0usize;
                for frame in bytes.chunks_exact(4 * channels) {
                    let mut v = 0.0;
                    for c in frame.chunks_exact(4) {
                        v += f32::from_le_bytes([c[0], c[1], c[2], c[3]]);
                    }
                    v /= channels as f32;
                    sum_sq += v * v;
                    n += 1;
                    s.samples.push(v);
                }
                if n > 0 {
                    level.store((sum_sq / n as f32).sqrt().to_bits(), Ordering::Relaxed);
                }
            }
        })
        .register()?;

    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_rate(SAMPLE_RATE);
    info.set_channels(1);
    let mut position = [0; spa::sys::SPA_AUDIO_MAX_CHANNELS as usize];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_MONO;
    info.set_position(position);
    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let bytes: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .map_err(|e| anyhow!("serializing format: {e:?}"))?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&bytes).ok_or_else(|| anyhow!("invalid format pod"))?];

    let mut flags = pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS;
    if !keep_warm {
        flags |= pw::stream::StreamFlags::INACTIVE;
    }
    stream.connect(spa::utils::Direction::Input, None, flags, &mut params)?;
    Ok(ActiveStream { stream, _listener: listener })
}
