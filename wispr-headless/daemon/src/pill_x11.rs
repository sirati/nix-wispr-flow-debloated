//! The flow bar on Linux (see bar.rs for what it shows).
//!
//! It is an X11 window (XWayland on GNOME and KDE): GNOME keeps no native
//! Wayland window above others, and an override-redirect X11 window is never
//! managed, focused or listed. Its input region is the drawn bar only, so
//! clicks next to it go through. A separate thread waits for X events, so a
//! resting bar costs no wakeups. If there is no X display, the bar is off.
//!
//! Sizes are multiplied by the display scale (Xft.dpi / 96, so 2 on a 200 %
//! screen).

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc as Shared;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    Arc, AtomEnum, ChangeGCAux, ClipOrdering, ColormapAlloc, ConnectionExt as _, CreateGCAux,
    CreateWindowAux, EventMask, Rectangle, Window, WindowClass,
};
use x11rb::protocol::Event as XEvent;
use x11rb::rust_connection::RustConnection;

use crate::bar::{
    click_action, dot_alpha, BarState, Frame, Mode, PillCommand, BAR_GAP, BORDER, BOTTOM_MARGIN,
    DOT, DOT_STEP, FRAME, WINDOW,
};
use crate::wave;

/// Display scale from an X resource string (`Xft.dpi: 192` gives 2).
pub fn scale_from_resources(resources: &str) -> f64 {
    resources
        .lines()
        .find_map(|l| l.strip_prefix("Xft.dpi:"))
        .and_then(|v| v.trim().parse::<f64>().ok())
        .map(|dpi| (dpi / 96.0).clamp(1.0, 4.0))
        .unwrap_or(1.0)
}

/// Device-pixel geometry for one display scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    pub scale: f64,
    pub width: u16,
    pub height: u16,
}

impl Geometry {
    pub fn new(scale: f64) -> Geometry {
        Geometry {
            scale,
            width: px(WINDOW.0, scale),
            height: px(WINDOW.1, scale),
        }
    }

    /// The part of the window a mode draws on, centred on the bottom line.
    pub fn body(&self, mode: Mode) -> Rectangle {
        let (w, h) = mode.size();
        let (w, h) = (px(w, self.scale), px(h, self.scale));
        Rectangle {
            x: ((self.width - w) / 2) as i16,
            y: (self.height - h) as i16,
            width: w,
            height: h,
        }
    }
}

fn px(css: f64, scale: f64) -> u16 {
    (css * scale).round().max(1.0) as u16
}

struct Surface {
    conn: Shared<RustConnection>,
    window: Window,
    gc: u32,
    mapped: bool,
    input: Option<(i16, i16, u16, u16)>,
    geo: Geometry,
}

/// Premultiplied ARGB, as a 32-bit visual expects.
fn argb(a: u8, r: u8, g: u8, b: u8) -> u32 {
    let pm = |c: u8| (u32::from(c) * u32::from(a) / 255) as u8;
    u32::from_be_bytes([a, pm(r), pm(g), pm(b)])
}

/// A rounded bar: two half circles and the rectangle between them.
fn capsule(r: Rectangle) -> (Vec<Arc>, Rectangle) {
    let d = r.height;
    let arcs = vec![
        Arc {
            x: r.x,
            y: r.y,
            width: d,
            height: d,
            angle1: 0,
            angle2: 360 * 64,
        },
        Arc {
            x: r.x + (r.width - d) as i16,
            y: r.y,
            width: d,
            height: d,
            angle1: 0,
            angle2: 360 * 64,
        },
    ];
    let mid = Rectangle {
        x: r.x + (d / 2) as i16,
        y: r.y,
        width: r.width - d,
        height: r.height,
    };
    (arcs, mid)
}

fn read_scale(conn: &RustConnection, root: Window) -> f64 {
    conn.get_property(
        false,
        root,
        AtomEnum::RESOURCE_MANAGER,
        AtomEnum::STRING,
        0,
        1 << 16,
    )
    .ok()
    .and_then(|c| c.reply().ok())
    .map(|r| scale_from_resources(&String::from_utf8_lossy(&r.value)))
    .unwrap_or(1.0)
}

impl Surface {
    fn open() -> Result<Surface, String> {
        let (conn, screen_num) = x11rb::connect(None).map_err(|e| format!("no X display: {e}"))?;
        let screen = conn.setup().roots[screen_num].clone();
        let visual = screen
            .allowed_depths
            .iter()
            .filter(|d| d.depth == 32)
            .flat_map(|d| d.visuals.iter())
            .find(|v| v.class == x11rb::protocol::xproto::VisualClass::TRUE_COLOR)
            .map(|v| v.visual_id)
            .ok_or("no 32-bit visual")?;
        let geo = Geometry::new(read_scale(&conn, screen.root));

        // Bottom centre of the primary monitor, or of the whole screen.
        let (mx, my, mw, mh) = conn
            .randr_get_monitors(screen.root, true)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| {
                let m = r
                    .monitors
                    .iter()
                    .find(|m| m.primary)
                    .or(r.monitors.first())?;
                Some((m.x, m.y, m.width, m.height))
            })
            .unwrap_or((0, 0, screen.width_in_pixels, screen.height_in_pixels));
        let x = mx + (mw as i16 - geo.width as i16) / 2;
        let y = my + mh as i16 - geo.height as i16 - px(BOTTOM_MARGIN, geo.scale) as i16;

        let colormap = conn.generate_id().map_err(|e| e.to_string())?;
        conn.create_colormap(ColormapAlloc::NONE, colormap, screen.root, visual)
            .map_err(|e| e.to_string())?;
        let window = conn.generate_id().map_err(|e| e.to_string())?;
        conn.create_window(
            32,
            window,
            screen.root,
            x,
            y,
            geo.width,
            geo.height,
            0,
            WindowClass::INPUT_OUTPUT,
            visual,
            &CreateWindowAux::new()
                .override_redirect(1)
                .background_pixel(0)
                .border_pixel(0)
                .colormap(colormap)
                .event_mask(EventMask::BUTTON_PRESS),
        )
        .map_err(|e| e.to_string())?;
        let gc = conn.generate_id().map_err(|e| e.to_string())?;
        conn.create_gc(gc, window, &CreateGCAux::new().foreground(0))
            .map_err(|e| e.to_string())?;
        conn.flush().map_err(|e| e.to_string())?;
        let mut s = Surface {
            conn: Shared::new(conn),
            window,
            gc,
            mapped: false,
            input: None,
            geo,
        };
        s.set_input(None);
        Ok(s)
    }

    /// Clicks are taken only on the drawn bar.
    fn set_input(&mut self, area: Option<Rectangle>) {
        let key = area.map(|r| (r.x, r.y, r.width, r.height));
        if key.is_some() && self.input == key {
            return;
        }
        let rects: Vec<Rectangle> = area.into_iter().collect();
        let _ = self.conn.shape_rectangles(
            shape::SO::SET,
            shape::SK::INPUT,
            ClipOrdering::UNSORTED,
            self.window,
            0,
            0,
            &rects,
        );
        self.input = key;
    }

    fn set_visible(&mut self, visible: bool) {
        if visible == self.mapped {
            return;
        }
        let _ = if visible {
            self.conn.map_window(self.window)
        } else {
            self.conn.unmap_window(self.window)
        };
        let _ = self.conn.flush();
        self.mapped = visible;
    }

    fn fill(&self, color: u32, rects: &[Rectangle]) {
        let _ = self
            .conn
            .change_gc(self.gc, &ChangeGCAux::new().foreground(color));
        let _ = self.conn.poly_fill_rectangle(self.window, self.gc, rects);
    }

    fn fill_arcs(&self, color: u32, arcs: &[Arc]) {
        let _ = self
            .conn
            .change_gc(self.gc, &ChangeGCAux::new().foreground(color));
        let _ = self.conn.poly_fill_arc(self.window, self.gc, arcs);
    }

    fn fill_capsule(&self, color: u32, r: Rectangle) {
        let (arcs, mid) = capsule(r);
        self.fill_arcs(color, &arcs);
        self.fill(color, &[mid]);
    }

    /// Wispr's outlined, translucent body. X writes pixels without blending,
    /// so the inner fill replaces the outline colour inside it.
    fn body(&self, area: Rectangle) {
        self.fill_capsule(argb(0xc0, 0xaa, 0xaa, 0xaa), area);
        let b = px(BORDER, self.geo.scale);
        let inner = Rectangle {
            x: area.x + b as i16,
            y: area.y + b as i16,
            width: area.width.saturating_sub(2 * b),
            height: area.height.saturating_sub(2 * b),
        };
        self.fill_capsule(argb(0x80, 0, 0, 0), inner);
    }

    fn draw(&mut self, f: &Frame) {
        let g = self.geo;
        self.fill(
            0,
            &[Rectangle {
                x: 0,
                y: 0,
                width: g.width,
                height: g.height,
            }],
        );
        let area = g.body(f.mode);
        self.set_input(Some(area));
        if f.mode == Mode::Hidden {
            let _ = self.conn.flush();
            return;
        }
        self.body(area);
        let cy = f64::from(area.y) + f64::from(area.height) / 2.0;
        match f.mode {
            Mode::Recording => {
                let bar_w = px(wave::BAR_WIDTH, g.scale);
                let gap = px(BAR_GAP, g.scale);
                let total = wave::BARS as u16 * bar_w + (wave::BARS as u16 - 1) * gap;
                let left = area.x + ((area.width - total) / 2) as i16;
                let alpha = if f.mic_active { 0xff } else { 0x66 };
                let rects: Vec<Rectangle> = f
                    .heights
                    .iter()
                    .enumerate()
                    .map(|(i, &h)| {
                        let hp =
                            px(h.max(wave::BAR_WIDTH), g.scale).min(area.height.saturating_sub(4));
                        Rectangle {
                            x: left + (i as u16 * (bar_w + gap)) as i16,
                            y: (cy - f64::from(hp) / 2.0).round() as i16,
                            width: bar_w,
                            height: hp,
                        }
                    })
                    .collect();
                self.fill(argb(alpha, 0xff, 0xff, 0xff), &rects);
            }
            Mode::Working => {
                let d = px(DOT, g.scale);
                let step = px(DOT_STEP, g.scale) as i16;
                let cx = area.x + (area.width / 2) as i16;
                for i in 0..3usize {
                    self.fill_arcs(
                        argb((dot_alpha(f.t, i) * 255.0) as u8, 0xff, 0xff, 0xff),
                        &[Arc {
                            x: cx + (i as i16 - 1) * step - (d / 2) as i16,
                            y: (cy - f64::from(d) / 2.0) as i16,
                            width: d,
                            height: d,
                            angle1: 0,
                            angle2: 360 * 64,
                        }],
                    );
                }
            }
            Mode::Resting | Mode::Hidden => {}
        }
        let _ = self.conn.flush();
    }
}

/// Runs the bar until the command channel closes. `on_click` gets "start" or
/// "stop" when the bar is clicked with the left button.
pub fn run(
    commands: Receiver<PillCommand>,
    log: impl Fn(String),
    on_click: impl Fn(&'static str) + Send + 'static,
) {
    let mut surface = match Surface::open() {
        Ok(s) => Some(s),
        Err(e) => {
            log(format!("flow bar off: {e}"));
            None
        }
    };
    // Clicks: a thread that blocks on X events. What a click means depends on
    // the mode, which it reads from here.
    let shown = Shared::new(std::sync::Mutex::new(Mode::Hidden));
    if let Some(s) = &surface {
        let conn = s.conn.clone();
        let shown = shown.clone();
        std::thread::spawn(move || {
            while let Ok(event) = conn.wait_for_event() {
                if let XEvent::ButtonPress(press) = event {
                    if press.detail != 1 {
                        continue;
                    }
                    let mode = *shown.lock().expect("mode lock poisoned");
                    if let Some(action) = click_action(mode) {
                        on_click(action);
                    }
                }
            }
        });
    }

    let mut state = BarState::default();
    let mut last_drawn: Option<Mode> = None;
    loop {
        // Only animated modes need frames; a resting or hidden bar sleeps.
        let timeout = if state.shown().animated() {
            FRAME
        } else {
            Duration::from_secs(3600)
        };
        match commands.recv_timeout(timeout) {
            Ok(c) => state.apply(c),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        let f = state.frame(Instant::now());
        *shown.lock().expect("mode lock poisoned") = f.mode;
        if let Some(s) = surface.as_mut() {
            let show = f.mode != Mode::Hidden;
            s.set_visible(show);
            if show && (f.mode.animated() || last_drawn != Some(f.mode)) {
                s.draw(&f);
                last_drawn = Some(f.mode);
            }
            if !show {
                last_drawn = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resting_bar_matches_wispr_at_200_percent() {
        let g = Geometry::new(2.0);
        let r = g.body(Mode::Resting);
        assert_eq!((r.width, r.height), (80, 16), "measured from Wispr's page");
        assert_eq!(r.y as u16 + r.height, g.height, "sits on the bottom line");
        assert_eq!(r.x as u16 * 2 + r.width, g.width, "centred");
        assert!(g.body(Mode::Recording).height > r.height);
    }

    #[test]
    fn display_scale_comes_from_xft_dpi() {
        assert_eq!(
            scale_from_resources("Xft.antialias:\t1\nXft.dpi:\t192\n"),
            2.0
        );
        assert_eq!(scale_from_resources("Xft.dpi: 96"), 1.0);
        assert_eq!(scale_from_resources(""), 1.0);
        assert_eq!(scale_from_resources("Xft.dpi: 9999"), 4.0);
    }

    #[test]
    fn colours_are_premultiplied() {
        assert_eq!(argb(0x80, 0xff, 0xff, 0xff), 0x8080_8080);
        assert_eq!(argb(0, 0xff, 0xff, 0xff), 0);
    }
}
