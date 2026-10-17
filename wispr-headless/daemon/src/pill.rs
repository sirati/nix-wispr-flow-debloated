//! Wispr's flow bar, drawn without Chromium: the small resting bar at the
//! bottom of the screen, Wispr's waveform while recording (see wave.rs), and
//! pulsing dots while Wispr finishes. A click on the resting bar starts a
//! dictation and a click while recording stops it, as on Wispr's own bar.
//!
//! It is an X11 window (XWayland on GNOME and KDE): GNOME keeps no native
//! Wayland window above others, and an override-redirect X11 window is never
//! managed, focused or listed. Its input region is the drawn bar only, so
//! clicks next to it go through. A separate thread waits for X events, so a
//! resting bar costs no wakeups. If there is no X display, the bar is off.
//!
//! Sizes are Wispr's CSS pixels, multiplied by the display scale (Xft.dpi /
//! 96, so 2 on a 200 % screen). Measured from Wispr's own page at 200 %: the
//! resting bar is 80x16 device pixels, a black fill at 50 % inside a 2 px
//! #aaaaaa outline at 75 %.

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

use crate::wave;

/// All sizes below are CSS pixels.
const WINDOW: (f64, f64) = (66.0, 18.0);
const REST: (f64, f64) = (40.0, 8.0);
const ACTIVE: (f64, f64) = (54.0, 18.0);
const BORDER: f64 = 1.0;
const BAR_GAP: f64 = 2.0;
/// Gap between the bottom of the bar and the bottom of the screen.
const BOTTOM_MARGIN: f64 = 10.0;
const FRAME: Duration = Duration::from_millis(16);

/// What the bar shows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    Hidden,
    Resting,
    Recording,
    Working,
}

/// From Wispr's dictation status and whether its resting bar is enabled.
pub fn mode_for(status: &str, resting_bar: bool) -> Mode {
    match status {
        "recording" => Mode::Recording,
        "idle" | "testing" | "dismissed" | "" => {
            if resting_bar {
                Mode::Resting
            } else {
                Mode::Hidden
            }
        }
        _ => Mode::Working,
    }
}

/// What a click on the bar means in each mode.
pub fn click_action(mode: Mode) -> Option<&'static str> {
    match mode {
        Mode::Resting => Some("start"),
        Mode::Recording => Some("stop"),
        Mode::Working | Mode::Hidden => None,
    }
}

/// Wispr's audio level is 0..1; tolerate dB just in case.
pub fn normalize_level(v: f64) -> f64 {
    if !v.is_finite() {
        0.0
    } else if v < 0.0 {
        ((v + 60.0) / 60.0).clamp(0.0, 1.0)
    } else {
        v.clamp(0.0, 1.0)
    }
}

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
        let (w, h) = match mode {
            Mode::Resting => REST,
            _ => ACTIVE,
        };
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

/// Commands for the bar thread.
pub enum PillCommand {
    Status(String),
    Level(f64),
    /// `status:micActive`: bars are white while the mic is active.
    Mic(bool),
    /// Wispr's own status page is up and draws the bar itself.
    Enabled(bool),
    /// Whether Wispr shows its resting bar (its hideFlowBarPermanently and
    /// snooze preferences).
    RestingBar(bool),
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

    fn draw(&mut self, mode: Mode, heights: &[f64; wave::BARS], mic_active: bool, phase: f64) {
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
        let area = g.body(mode);
        self.set_input(Some(area));
        if mode == Mode::Hidden {
            let _ = self.conn.flush();
            return;
        }
        self.body(area);
        let cy = f64::from(area.y) + f64::from(area.height) / 2.0;
        match mode {
            Mode::Recording => {
                let bar_w = px(wave::BAR_WIDTH, g.scale);
                let gap = px(BAR_GAP, g.scale);
                let total = wave::BARS as u16 * bar_w + (wave::BARS as u16 - 1) * gap;
                let left = area.x + ((area.width - total) / 2) as i16;
                let alpha = if mic_active { 0xff } else { 0x66 };
                let rects: Vec<Rectangle> = heights
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
                let d = px(3.0, g.scale);
                let step = px(6.0, g.scale) as i16;
                let cx = area.x + (area.width / 2) as i16;
                for i in 0..3i16 {
                    let pulse = ((phase * 6.0 - f64::from(i) * 0.9).sin() * 0.5 + 0.5) * 0.8 + 0.2;
                    self.fill_arcs(
                        argb((pulse * 255.0) as u8, 0xff, 0xff, 0xff),
                        &[Arc {
                            x: cx + (i - 1) * step - (d / 2) as i16,
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

    let mut status = String::from("idle");
    let mut resting_bar = false;
    let mut enabled = true;
    let mut mic_active = false;
    let mut target = 0.0f64;
    let mut level = 0.0f64;
    let mut last_frame = Instant::now();
    let mut last_drawn: Option<Mode> = None;
    let start = Instant::now();
    loop {
        let mode = mode_for(&status, resting_bar);
        // Only animated modes need frames; a resting or hidden bar sleeps.
        let animated = enabled && matches!(mode, Mode::Recording | Mode::Working);
        let timeout = if animated {
            FRAME
        } else {
            Duration::from_secs(3600)
        };
        match commands.recv_timeout(timeout) {
            Ok(PillCommand::Status(s)) => {
                status = s;
                if status != "recording" {
                    target = 0.0;
                }
            }
            Ok(PillCommand::Level(v)) => target = normalize_level(v),
            Ok(PillCommand::Mic(m)) => mic_active = m,
            Ok(PillCommand::Enabled(e)) => enabled = e,
            Ok(PillCommand::RestingBar(r)) => resting_bar = r,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        let now = Instant::now();
        level = wave::smooth(
            level,
            target,
            now.duration_since(last_frame).as_secs_f64() * 1000.0,
        );
        last_frame = now;

        let mode = mode_for(&status, resting_bar);
        let show = enabled && mode != Mode::Hidden;
        *shown.lock().expect("mode lock poisoned") = if show { mode } else { Mode::Hidden };
        if let Some(s) = surface.as_mut() {
            s.set_visible(show);
            let animated = matches!(mode, Mode::Recording | Mode::Working);
            if show && (animated || last_drawn != Some(mode)) {
                let t = start.elapsed().as_secs_f64();
                s.draw(mode, &wave::heights(t, level), mic_active, t);
                last_drawn = Some(mode);
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
    fn statuses_map_to_modes() {
        assert_eq!(mode_for("recording", true), Mode::Recording);
        assert_eq!(mode_for("idle", true), Mode::Resting);
        assert_eq!(mode_for("idle", false), Mode::Hidden);
        assert_eq!(mode_for("flushing", false), Mode::Working);
        assert_eq!(mode_for("compressing", true), Mode::Working);
        assert_eq!(mode_for("reconnecting", true), Mode::Working);
        assert_eq!(mode_for("something-new", true), Mode::Working);
    }

    #[test]
    fn clicks_start_from_rest_and_stop_a_recording() {
        assert_eq!(click_action(Mode::Resting), Some("start"));
        assert_eq!(click_action(Mode::Recording), Some("stop"));
        assert_eq!(click_action(Mode::Working), None);
        assert_eq!(click_action(Mode::Hidden), None);
    }

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
    fn levels_are_clamped_and_db_tolerated() {
        assert_eq!(normalize_level(0.5), 0.5);
        assert_eq!(normalize_level(3.0), 1.0);
        assert_eq!(normalize_level(f64::NAN), 0.0);
        assert!((normalize_level(-30.0) - 0.5).abs() < 1e-9);
        assert_eq!(normalize_level(-200.0), 0.0);
    }

    #[test]
    fn colours_are_premultiplied() {
        assert_eq!(argb(0x80, 0xff, 0xff, 0xff), 0x8080_8080);
        assert_eq!(argb(0, 0xff, 0xff, 0xff), 0);
    }
}
