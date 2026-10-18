//! Recording indicator: a small pill near the mouse pointer.
//!
//! An override-redirect X11 window (never managed by the WM, so it can never
//! take keyboard focus away from the app we paste into), with an empty input
//! shape so clicks pass through. Drawn with tiny-skia and only animated while
//! visible.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel as cb;
use tiny_skia::{Color, FillRule, Paint, Path, PathBuilder, Pixmap, Rect, Transform};
use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    ColormapAlloc, ConfigureWindowAux, ConnectionExt as _, CreateGCAux, CreateWindowAux,
    ImageFormat, PropMode, StackMode, VisualClass, Window, WindowClass, AtomEnum,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

const W: u16 = 116;
const H: u16 = 36;
const FRAME: Duration = Duration::from_millis(33);
const BARS: usize = 9;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverlayState {
    Hidden,
    Recording,
    Transcribing,
}

pub enum OverlayCmd {
    Set(OverlayState),
    /// Highlight that one more Esc cancels.
    ConfirmCancel(bool),
}

pub fn spawn(level: Arc<AtomicU32>) -> Result<cb::Sender<OverlayCmd>> {
    let (tx, rx) = cb::unbounded();
    let mut overlay = Overlay::new(level)?;
    std::thread::Builder::new().name("overlay".into()).spawn(move || overlay.run(rx))?;
    Ok(tx)
}

struct Overlay {
    conn: RustConnection,
    root: Window,
    window: Window,
    gc: u32,
    depth: u8,
    argb: bool,
    pixmap: Pixmap,
    level: Arc<AtomicU32>,
    levels: VecDeque<f32>,
    state: OverlayState,
    confirm: bool,
    shown_at: Instant,
}

impl Overlay {
    fn new(level: Arc<AtomicU32>) -> Result<Self> {
        let (conn, screen_num) = x11rb::connect(None).context("connecting to the X server")?;
        let screen = conn.setup().roots[screen_num].clone();

        // Rounded, translucent corners need a 32-bit visual and a running
        // compositor; otherwise fall back to an opaque rectangle.
        let composited = {
            let name = format!("_NET_WM_CM_S{screen_num}");
            let atom = conn.intern_atom(false, name.as_bytes())?.reply()?.atom;
            conn.get_selection_owner(atom)?.reply()?.owner != x11rb::NONE
        };
        let argb_visual = screen
            .allowed_depths
            .iter()
            .filter(|d| d.depth == 32)
            .flat_map(|d| d.visuals.iter())
            .find(|v| v.class == VisualClass::TRUE_COLOR)
            .map(|v| v.visual_id);
        let (depth, visual, argb) = match (composited, argb_visual) {
            (true, Some(v)) => (32, v, true),
            _ => (screen.root_depth, screen.root_visual, false),
        };

        let colormap = conn.generate_id()?;
        conn.create_colormap(ColormapAlloc::NONE, colormap, screen.root, visual)?;
        let window = conn.generate_id()?;
        conn.create_window(
            depth,
            window,
            screen.root,
            0,
            0,
            W,
            H,
            0,
            WindowClass::INPUT_OUTPUT,
            visual,
            &CreateWindowAux::new()
                .override_redirect(1)
                .background_pixel(0)
                .border_pixel(0)
                .colormap(colormap),
        )?;
        // Click-through: empty input region.
        conn.shape_rectangles(
            shape::SO::SET,
            shape::SK::INPUT,
            x11rb::protocol::xproto::ClipOrdering::UNSORTED,
            window,
            0,
            0,
            &[],
        )?;
        let type_atom = conn.intern_atom(false, b"_NET_WM_WINDOW_TYPE")?.reply()?.atom;
        let notification = conn
            .intern_atom(false, b"_NET_WM_WINDOW_TYPE_NOTIFICATION")?
            .reply()?
            .atom;
        conn.change_property32(PropMode::REPLACE, window, type_atom, AtomEnum::ATOM, &[notification])?;
        conn.change_property8(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            b"OpenSuperWhisper indicator",
        )?;
        let gc = conn.generate_id()?;
        conn.create_gc(gc, window, &CreateGCAux::new())?;
        conn.flush()?;

        Ok(Self {
            conn,
            root: screen.root,
            window,
            gc,
            depth,
            argb,
            pixmap: Pixmap::new(W as u32, H as u32).context("pixmap")?,
            level,
            levels: VecDeque::from(vec![0.0; BARS]),
            state: OverlayState::Hidden,
            confirm: false,
            shown_at: Instant::now(),
        })
    }

    fn run(&mut self, rx: cb::Receiver<OverlayCmd>) {
        let mut next_frame = Instant::now();
        loop {
            let cmd = if self.state == OverlayState::Hidden {
                rx.recv().ok()
            } else {
                match rx.recv_deadline(next_frame) {
                    Ok(cmd) => Some(cmd),
                    Err(cb::RecvTimeoutError::Timeout) => None,
                    Err(cb::RecvTimeoutError::Disconnected) => return,
                }
            };
            match cmd {
                Some(OverlayCmd::Set(state)) => self.set_state(state),
                Some(OverlayCmd::ConfirmCancel(on)) => self.confirm = on,
                None if self.state == OverlayState::Hidden => return,
                None => {}
            }
            if self.state != OverlayState::Hidden && Instant::now() >= next_frame {
                if let Err(e) = self.draw() {
                    log::warn!("overlay draw failed: {e}");
                }
                next_frame = Instant::now() + FRAME;
            }
        }
    }

    fn set_state(&mut self, state: OverlayState) {
        if state == self.state {
            return;
        }
        let was_hidden = self.state == OverlayState::Hidden;
        self.state = state;
        self.confirm = false;
        let result = match (was_hidden, state) {
            (true, OverlayState::Hidden) | (false, OverlayState::Recording | OverlayState::Transcribing) => Ok(()),
            (false, OverlayState::Hidden) => self.conn.unmap_window(self.window).map(drop),
            (true, _) => {
                self.levels.iter_mut().for_each(|l| *l = 0.0);
                self.shown_at = Instant::now();
                self.show()
            }
        };
        if let Err(e) = result.map_err(anyhow::Error::from).and_then(|_| Ok(self.conn.flush()?)) {
            log::warn!("overlay update failed: {e}");
        }
    }

    fn show(&self) -> Result<(), x11rb::errors::ConnectionError> {
        let (x, y) = self.position().unwrap_or((100, 100));
        self.conn.configure_window(
            self.window,
            &ConfigureWindowAux::new().x(x).y(y).stack_mode(StackMode::ABOVE),
        )?;
        self.conn.map_window(self.window)?;
        Ok(())
    }

    /// Below the pointer, horizontally centered, kept inside its monitor.
    fn position(&self) -> Option<(i32, i32)> {
        let pointer = self.conn.query_pointer(self.root).ok()?.reply().ok()?;
        let (px, py) = (pointer.root_x as i32, pointer.root_y as i32);
        let (mut mx, mut my, mut mw, mut mh) = (0, 0, i32::MAX, i32::MAX);
        if let Ok(Ok(monitors)) = self.conn.randr_get_monitors(self.root, true).map(|c| c.reply()) {
            if let Some(m) = monitors.monitors.iter().find(|m| {
                let (x, y, w, h) = (m.x as i32, m.y as i32, m.width as i32, m.height as i32);
                px >= x && px < x + w && py >= y && py < y + h
            }) {
                (mx, my, mw, mh) = (m.x as i32, m.y as i32, m.width as i32, m.height as i32);
            }
        }
        let (w, h) = (W as i32, H as i32);
        let x = (px - w / 2).clamp(mx + 8, (mx + mw - w - 8).max(mx + 8));
        let below = py + 28;
        let y = if below + h <= my.saturating_add(mh) - 8 { below } else { py - 28 - h };
        Some((x, y.max(my + 8)))
    }

    fn draw(&mut self) -> Result<()> {
        self.levels.pop_front();
        self.levels.push_back(f32::from_bits(self.level.load(Ordering::Relaxed)));
        let t = self.shown_at.elapsed().as_secs_f32();
        render(&mut self.pixmap, self.state, self.confirm, &self.levels, t, self.argb);

        // tiny-skia is premultiplied RGBA; X wants premultiplied BGRA.
        let mut data = self.pixmap.data().to_vec();
        for px in data.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        self.conn.put_image(
            ImageFormat::Z_PIXMAP,
            self.window,
            self.gc,
            W,
            H,
            0,
            0,
            0,
            self.depth,
            &data,
        )?;
        self.conn.flush()?;
        Ok(())
    }
}

fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<Path> {
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish()
}

fn paint(r: u8, g: u8, b: u8, a: u8) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8(r, g, b, a);
    p.anti_alias = true;
    p
}

fn render(pm: &mut Pixmap, state: OverlayState, confirm: bool, levels: &VecDeque<f32>, t: f32, argb: bool) {
    let (w, h) = (W as f32, H as f32);
    pm.fill(Color::TRANSPARENT);
    let bg = if confirm { paint(150, 70, 10, 235) } else { paint(24, 24, 28, 235) };
    if argb {
        if let Some(p) = rounded_rect(0.5, 0.5, w - 1.0, h - 1.0, h / 2.0) {
            pm.fill_path(&p, &bg, FillRule::Winding, Transform::identity(), None);
        }
    } else if let Some(r) = Rect::from_xywh(0.0, 0.0, w, h) {
        pm.fill_rect(r, &bg, Transform::identity(), None);
    }

    let cy = h / 2.0;
    match state {
        OverlayState::Hidden => {}
        OverlayState::Recording => {
            // Pulsing red dot.
            let pulse = 0.85 + 0.15 * (t * 4.0).sin();
            if let Some(p) = PathBuilder::from_circle(20.0, cy, 6.0 * pulse) {
                pm.fill_path(&p, &paint(239, 68, 68, 255), FillRule::Winding, Transform::identity(), None);
            }
            // Level bars, newest on the right. Speech RMS is ~0.01-0.2, so
            // map logarithmically.
            let (x0, bar_w, gap) = (36.0, 5.0, 3.5);
            for (i, &level) in levels.iter().enumerate() {
                let db = 20.0 * level.max(1e-5).log10();
                let norm = ((db + 55.0) / 45.0).clamp(0.0, 1.0);
                let bh = 4.0 + norm * (h - 14.0);
                let x = x0 + i as f32 * (bar_w + gap);
                if let Some(p) = rounded_rect(x, cy - bh / 2.0, bar_w, bh, bar_w / 2.0) {
                    pm.fill_path(&p, &paint(240, 240, 245, 230), FillRule::Winding, Transform::identity(), None);
                }
            }
        }
        OverlayState::Transcribing => {
            // Three dots in a travelling wave.
            for i in 0..3 {
                let phase = t * 6.0 - i as f32 * 0.9;
                let lift = phase.sin().max(0.0) * 5.0;
                let alpha = (150.0 + 105.0 * phase.sin().max(0.0)) as u8;
                if let Some(p) = PathBuilder::from_circle(w / 2.0 - 16.0 + i as f32 * 16.0, cy - lift + 2.0, 4.0) {
                    pm.fill_path(&p, &paint(240, 240, 245, alpha), FillRule::Winding, Transform::identity(), None);
                }
            }
        }
    }
}
