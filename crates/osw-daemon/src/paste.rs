//! Inserting text into the focused app: clipboard + synthesized Ctrl+V.
//!
//! Runs on its own thread, which owns the clipboard. The previous clipboard
//! text is snapshotted while the transcription is still running (so the
//! slow round-trip to the clipboard owner is off the critical path) and put
//! back after the target app had time to service the paste.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel as cb;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt, Keycode, Window};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use crate::keys::{self, Keymap};

/// Slow consumers (browsers, Electron apps) can service the synthesized
/// paste long after the event is sent. Restoring earlier makes them paste
/// the old contents.
const RESTORE_DELAY: Duration = Duration::from_millis(1500);

const TERMINALS: &[&str] = &[
    "terminal", "xterm", "urxvt", "rxvt", "kitty", "alacritty", "konsole", "tilix",
    "terminator", "wezterm", "foot", "guake", "tilda", "yakuake", "st-256color", "sakura",
    "terminology", "ghostty", "warp", "contour", "rio",
];

pub enum PasteCmd {
    /// Remember the current clipboard text so it can be restored.
    Snapshot,
    /// Insert text; `paste == false` only copies.
    Insert { text: String, paste: bool, keep_in_clipboard: bool },
}

pub fn spawn() -> Result<cb::Sender<PasteCmd>> {
    let (tx, rx) = cb::unbounded();
    let mut paster = Paster::new()?;
    std::thread::Builder::new().name("paste".into()).spawn(move || paster.run(rx))?;
    Ok(tx)
}

struct Paster {
    conn: RustConnection,
    root: Window,
    clipboard: arboard::Clipboard,
    snapshot: Option<Option<String>>,
    /// (time, text we set, text to restore)
    pending_restore: Option<(Instant, String, Option<String>)>,
}

impl Paster {
    fn new() -> Result<Self> {
        let (conn, screen) = x11rb::connect(None).context("connecting to the X server")?;
        let root = conn.setup().roots[screen].root;
        let clipboard = arboard::Clipboard::new().context("opening the clipboard")?;
        Ok(Self { conn, root, clipboard, snapshot: None, pending_restore: None })
    }

    fn run(&mut self, rx: cb::Receiver<PasteCmd>) {
        loop {
            let cmd = match &self.pending_restore {
                Some((at, ..)) => match rx.recv_deadline(*at) {
                    Ok(cmd) => Some(cmd),
                    Err(cb::RecvTimeoutError::Timeout) => None,
                    Err(cb::RecvTimeoutError::Disconnected) => return,
                },
                None => match rx.recv() {
                    Ok(cmd) => Some(cmd),
                    Err(_) => return,
                },
            };
            match cmd {
                None => self.restore(),
                Some(PasteCmd::Snapshot) => {
                    // A restore still pending holds the user's real clipboard.
                    let previous = match &self.pending_restore {
                        Some((_, _, saved)) => saved.clone(),
                        None => self.clipboard.get_text().ok(),
                    };
                    self.snapshot = Some(previous);
                }
                Some(PasteCmd::Insert { text, paste, keep_in_clipboard }) => {
                    if let Err(e) = self.insert(&text, paste, keep_in_clipboard) {
                        log::error!("inserting text failed: {e:#}");
                    }
                }
            }
        }
    }

    fn insert(&mut self, text: &str, paste: bool, keep: bool) -> Result<()> {
        let saved = match self.snapshot.take() {
            Some(saved) => saved,
            None => match self.pending_restore.take() {
                Some((_, _, saved)) => saved,
                None => self.clipboard.get_text().ok(),
            },
        };
        self.pending_restore = None;
        self.clipboard.set_text(text).context("setting clipboard")?;
        if !paste {
            return Ok(());
        }
        let t = Instant::now();
        self.send_paste_keys()?;
        log::debug!("paste keys sent in {:?}", t.elapsed());
        if !keep {
            self.pending_restore = Some((Instant::now() + RESTORE_DELAY, text.to_string(), saved));
        }
        Ok(())
    }

    fn restore(&mut self) {
        let Some((_, ours, saved)) = self.pending_restore.take() else { return };
        // Only restore if the clipboard still holds our text: otherwise the
        // user copied something new and restoring would clobber it.
        if self.clipboard.get_text().ok().as_deref() != Some(ours.as_str()) {
            return;
        }
        let result = match saved {
            Some(saved) => self.clipboard.set_text(saved),
            None => self.clipboard.clear(),
        };
        if let Err(e) = result {
            log::warn!("restoring clipboard failed: {e}");
        }
    }

    fn send_paste_keys(&self) -> Result<()> {
        let keymap = Keymap::load(&self.conn)?;
        let ctrl = keymap.keycode(keys::XK_CONTROL_L).context("no Control key")?;
        let shift = keymap.keycode(keys::XK_SHIFT_L).context("no Shift key")?;
        let v = keymap.keycode(keys::XK_V).context("no V key")?;
        let terminal = self.focused_is_terminal();

        // Modifiers the user still holds (e.g. Alt of the trigger shortcut)
        // would turn Ctrl+V into something else: release them for the
        // duration of the paste and press them again afterwards.
        let held = self.held_modifiers(&keymap)?;
        for &kc in &held {
            self.key(kc, false)?;
        }
        self.key(ctrl, true)?;
        if terminal {
            self.key(shift, true)?;
        }
        self.key(v, true)?;
        self.key(v, false)?;
        if terminal {
            self.key(shift, false)?;
        }
        self.key(ctrl, false)?;
        for &kc in &held {
            self.key(kc, true)?;
        }
        // Round-trip so the events are processed before we return.
        self.conn.get_input_focus()?.reply()?;
        Ok(())
    }

    fn key(&self, keycode: Keycode, press: bool) -> Result<()> {
        let kind = if press {
            x11rb::protocol::xproto::KEY_PRESS_EVENT
        } else {
            x11rb::protocol::xproto::KEY_RELEASE_EVENT
        };
        self.conn.xtest_fake_input(kind, keycode, 0, self.root, 0, 0, 0)?;
        Ok(())
    }

    fn held_modifiers(&self, keymap: &Keymap) -> Result<Vec<Keycode>> {
        let pressed = self.conn.query_keymap()?.reply()?.keys;
        let is_down = |kc: Keycode| pressed[kc as usize / 8] & (1 << (kc % 8)) != 0;
        let mut held: Vec<Keycode> = [
            keys::XK_CONTROL_L, keys::XK_CONTROL_R, keys::XK_SHIFT_L, keys::XK_SHIFT_R,
            keys::XK_ALT_L, keys::XK_ALT_R, keys::XK_SUPER_L, keys::XK_SUPER_R,
            keys::XK_ISO_LEVEL3_SHIFT,
        ]
        .iter()
        .flat_map(|&sym| keymap.keycodes(sym))
        .filter(|&kc| is_down(kc))
        .collect();
        held.dedup();
        Ok(held)
    }

    fn focused_is_terminal(&self) -> bool {
        self.active_window_class()
            .map(|class| {
                let class = class.to_ascii_lowercase();
                TERMINALS.iter().any(|t| class.contains(t))
            })
            .unwrap_or(false)
    }

    fn active_window_class(&self) -> Option<String> {
        let atom = |name: &str| self.conn.intern_atom(true, name.as_bytes()).ok()?.reply().ok();
        let active = atom("_NET_ACTIVE_WINDOW")?.atom;
        let window = self
            .conn
            .get_property(false, self.root, active, AtomEnum::WINDOW, 0, 1)
            .ok()?
            .reply()
            .ok()?
            .value32()?
            .next()?;
        let class = self
            .conn
            .get_property(false, window, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)
            .ok()?
            .reply()
            .ok()?;
        // WM_CLASS is "instance\0class\0".
        Some(String::from_utf8_lossy(&class.value).replace('\0', " "))
    }
}
