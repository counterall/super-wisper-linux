//! Global recording triggers on X11.
//!
//! - Key combinations use passive key grabs (the keystroke is consumed).
//! - A single modifier key is watched through XInput2 raw events, so the key
//!   keeps working normally; pressing any other key or button while it is
//!   held reports a [`InputEvent::Chord`] (the user was typing a shortcut).
//! - Mouse buttons use passive button grabs.
//! - Esc is grabbed only while a recording or transcription is active.

use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xkb::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    ButtonIndex, ConnectionExt as _, EventMask, GrabMode, Keycode, ModMask, Window,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

use osw_core::config::{ModifierKey, Trigger};

use crate::keys::{self, Keymap};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputEvent {
    TriggerDown,
    TriggerUp,
    /// Another key was pressed while a modifier-only trigger was held.
    Chord,
    Escape,
}

enum Active {
    None,
    Shortcut { keycodes: Vec<Keycode>, mods: ModMask },
    Modifier { keycodes: Vec<Keycode> },
    Mouse { button: u8 },
}

struct Shared {
    active: Active,
    trigger_down: bool,
    escape_keycodes: Vec<Keycode>,
    escape_grabbed: bool,
}

pub struct Input {
    conn: Arc<RustConnection>,
    root: Window,
    shared: Arc<Mutex<Shared>>,
}

impl Input {
    pub fn spawn(trigger: &Trigger, on_event: impl Fn(InputEvent) + Send + 'static) -> Result<Self> {
        let (conn, screen) = x11rb::connect(None).context("connecting to the X server")?;
        let conn = Arc::new(conn);
        let root = conn.setup().roots[screen].root;

        // Without detectable auto-repeat a held key produces fake
        // release/press pairs, which would break hold-to-record.
        conn.xkb_use_extension(1, 0)?.reply()?;
        let flag = xkb::PerClientFlag::DETECTABLE_AUTO_REPEAT;
        conn.xkb_per_client_flags(
            xkb::ID::USE_CORE_KBD.into(),
            flag,
            flag,
            xkb::BoolCtrl::from(0u32),
            xkb::BoolCtrl::from(0u32),
            xkb::BoolCtrl::from(0u32),
        )?
        .reply()?;

        conn.xinput_xi_query_version(2, 0)?.reply().context("XInput2 is required")?;

        let keymap = Keymap::load(&*conn)?;
        let shared = Arc::new(Mutex::new(Shared {
            active: Active::None,
            trigger_down: false,
            escape_keycodes: keymap.keycodes(keys::XK_ESCAPE),
            escape_grabbed: false,
        }));
        let input = Self { conn, root, shared };
        input.set_trigger(trigger)?;

        let conn = input.conn.clone();
        let shared = input.shared.clone();
        thread::Builder::new()
            .name("x11-input".into())
            .spawn(move || event_loop(&conn, &shared, &on_event))?;
        Ok(input)
    }

    /// Replaces the active trigger, releasing the previous grabs.
    pub fn set_trigger(&self, trigger: &Trigger) -> Result<()> {
        let mut shared = self.shared.lock().unwrap();
        self.release(&shared.active)?;
        shared.active = Active::None;
        shared.trigger_down = false;

        let keymap = Keymap::load(&*self.conn)?;
        shared.active = match trigger {
            Trigger::None => Active::None,
            Trigger::Shortcut { keys } => {
                let sc = keys::parse_shortcut(keys)?;
                let keycodes = keymap.keycodes(sc.keysym);
                anyhow::ensure!(!keycodes.is_empty(), "no key on this keyboard produces {keys:?}");
                for &kc in &keycodes {
                    for extra in keys::lock_variants() {
                        self.conn
                            .grab_key(false, self.root, sc.mods | extra, kc, GrabMode::ASYNC, GrabMode::ASYNC)?
                            .check()
                            .with_context(|| format!("shortcut {keys:?} is already taken by another app"))?;
                    }
                }
                Active::Shortcut { keycodes, mods: sc.mods }
            }
            Trigger::ModifierKey { key } => {
                let keycodes: Vec<Keycode> =
                    modifier_keysyms(*key).iter().flat_map(|&s| keymap.keycodes(s)).collect();
                anyhow::ensure!(!keycodes.is_empty(), "no key on this keyboard produces {key:?}");
                self.select_raw_events(true)?;
                Active::Modifier { keycodes }
            }
            Trigger::MouseButton { button } => {
                self.conn
                    .grab_button(
                        false,
                        self.root,
                        EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE,
                        GrabMode::ASYNC,
                        GrabMode::ASYNC,
                        x11rb::NONE,
                        x11rb::NONE,
                        ButtonIndex::from(*button),
                        ModMask::ANY,
                    )?
                    .check()
                    .with_context(|| format!("mouse button {button} is already grabbed"))?;
                Active::Mouse { button: *button }
            }
        };
        self.conn.flush()?;
        Ok(())
    }

    fn release(&self, active: &Active) -> Result<()> {
        match active {
            Active::None => {}
            Active::Shortcut { keycodes, mods } => {
                for &kc in keycodes {
                    for extra in keys::lock_variants() {
                        self.conn.ungrab_key(kc, self.root, *mods | extra)?;
                    }
                }
            }
            Active::Modifier { .. } => self.select_raw_events(false)?,
            Active::Mouse { button } => {
                self.conn.ungrab_button(ButtonIndex::from(*button), self.root, ModMask::ANY)?;
            }
        }
        Ok(())
    }

    fn select_raw_events(&self, on: bool) -> Result<()> {
        let mask = if on {
            xinput::XIEventMask::RAW_KEY_PRESS
                | xinput::XIEventMask::RAW_KEY_RELEASE
                | xinput::XIEventMask::RAW_BUTTON_PRESS
        } else {
            xinput::XIEventMask::from(0u32)
        };
        self.conn.xinput_xi_select_events(
            self.root,
            &[xinput::EventMask {
                deviceid: xinput::Device::ALL_MASTER.into(),
                mask: vec![mask],
            }],
        )?;
        Ok(())
    }

    /// Grabs Esc while something can be cancelled, so the keystroke does not
    /// reach the focused app.
    pub fn grab_escape(&self, grab: bool) {
        let mut shared = self.shared.lock().unwrap();
        if shared.escape_grabbed == grab {
            return;
        }
        shared.escape_grabbed = grab;
        for &kc in &shared.escape_keycodes {
            for extra in keys::lock_variants() {
                let result = if grab {
                    self.conn
                        .grab_key(false, self.root, extra, kc, GrabMode::ASYNC, GrabMode::ASYNC)
                        .map(drop)
                } else {
                    self.conn.ungrab_key(kc, self.root, extra).map(drop)
                };
                if let Err(e) = result {
                    log::warn!("escape grab: {e}");
                }
            }
        }
        let _ = self.conn.flush();
    }
}

fn modifier_keysyms(key: ModifierKey) -> &'static [u32] {
    match key {
        ModifierKey::LeftCtrl => &[keys::XK_CONTROL_L],
        ModifierKey::RightCtrl => &[keys::XK_CONTROL_R],
        ModifierKey::LeftShift => &[keys::XK_SHIFT_L],
        ModifierKey::RightShift => &[keys::XK_SHIFT_R],
        ModifierKey::LeftAlt => &[keys::XK_ALT_L],
        ModifierKey::RightAlt => &[keys::XK_ALT_R, keys::XK_ISO_LEVEL3_SHIFT],
        ModifierKey::LeftSuper => &[keys::XK_SUPER_L],
        ModifierKey::RightSuper => &[keys::XK_SUPER_R],
    }
}

fn event_loop(conn: &RustConnection, shared: &Mutex<Shared>, on_event: &dyn Fn(InputEvent)) {
    loop {
        let event = match conn.wait_for_event() {
            Ok(e) => e,
            Err(e) => {
                log::error!("X connection lost: {e}");
                return;
            }
        };
        if let Event::MappingNotify(_) = event {
            // Layout changed: refresh Esc now; trigger keycodes are
            // re-resolved on the next trigger change.
            if let Ok(km) = Keymap::load(conn) {
                shared.lock().unwrap().escape_keycodes = km.keycodes(keys::XK_ESCAPE);
            }
            continue;
        }
        if let Event::Error(e) = &event {
            log::warn!("X error: {e:?}");
            continue;
        }

        let mut s = shared.lock().unwrap();
        let Some(ev) = translate(&s, &event) else { continue };
        match ev {
            InputEvent::TriggerDown => s.trigger_down = true,
            InputEvent::TriggerUp => s.trigger_down = false,
            InputEvent::Chord | InputEvent::Escape => {}
        }
        drop(s);
        on_event(ev);
    }
}

fn translate(s: &Shared, event: &Event) -> Option<InputEvent> {
    let down = s.trigger_down;
    match (&s.active, event) {
        (_, Event::KeyPress(e)) if s.escape_grabbed && s.escape_keycodes.contains(&e.detail) => {
            Some(InputEvent::Escape)
        }
        (Active::Shortcut { keycodes, mods }, Event::KeyPress(e)) => {
            let state = ModMask::from(u16::from(e.state)) & keys::significant_mods();
            (keycodes.contains(&e.detail) && state == *mods && !down).then_some(InputEvent::TriggerDown)
        }
        (Active::Shortcut { keycodes, .. }, Event::KeyRelease(e)) => {
            (keycodes.contains(&e.detail) && down).then_some(InputEvent::TriggerUp)
        }
        (Active::Modifier { keycodes }, Event::XinputRawKeyPress(e)) => {
            let kc = e.detail as Keycode;
            if keycodes.contains(&kc) {
                (!down).then_some(InputEvent::TriggerDown)
            } else {
                (down && !s.escape_keycodes.contains(&kc)).then_some(InputEvent::Chord)
            }
        }
        (Active::Modifier { keycodes }, Event::XinputRawKeyRelease(e)) => {
            (keycodes.contains(&(e.detail as Keycode)) && down).then_some(InputEvent::TriggerUp)
        }
        (Active::Modifier { .. }, Event::XinputRawButtonPress(_)) => down.then_some(InputEvent::Chord),
        (Active::Mouse { button }, Event::ButtonPress(e)) if e.detail == *button => {
            (!down).then_some(InputEvent::TriggerDown)
        }
        (Active::Mouse { button }, Event::ButtonRelease(e)) if e.detail == *button => {
            down.then_some(InputEvent::TriggerUp)
        }
        _ => None,
    }
}
