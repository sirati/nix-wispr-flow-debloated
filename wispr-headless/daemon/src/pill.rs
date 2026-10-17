//! Wispr's flow bar, drawn without Chromium: the small resting bar at the
//! bottom of the screen, level bars while recording, and pulsing dots while
//! Wispr finishes. A click on the resting bar starts a dictation and a click
//! while recording stops it, as on Wispr's own bar.
//!
//! It is an X11 window (XWayland on GNOME and KDE): GNOME keeps no native
//! Wayland window above others, and an override-redirect X11 window is never
//! managed, focused or listed. Its input region is the drawn bar only, so
//! clicks next to it go through. A separate thread waits for X events, so a
//! resting bar costs no wakeups. If there is no X display, the bar is off.

use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc as Shared;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    Arc, ChangeGCAux, ClipOrdering, ColormapAlloc, ConnectionExt as _, CreateGCAux,
    CreateWindowAux, EventMask, Rectangle, Window, WindowClass,
};
use x11rb::protocol::Event as XEvent;
use x11rb::rust_connection::RustConnection;

/// The window: large enough for the recording bar.
pub const WIDTH: u16 = 132;
pub const HEIGHT: u16 = 36;
/// The resting bar, as Wispr draws it.
pub const REST_WIDTH: u16 = 78;
pub const REST_HEIGHT: u16 = 18;
const REST_BORDER: u16 = 2;
pub const BARS: usize = 13;
const BAR_WIDTH: i16 = 4;
const BAR_GAP: i16 = 4;
/// Gap between the bottom of the bars and the bottom of the screen.
const BOTTOM_MARGIN: i16 = 20;
const FRAME: Duration = Duration::from_millis(33);

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

/// Bar heights in pixels, newest level in the middle, older ones spreading out.
pub fn bar_heights(levels: &VecDeque<f64>, max: i16) -> [i16; BARS] {
    let mut out = [3i16; BARS];
    let mid = BARS / 2;
    for (age, level) in levels.iter().rev().take(mid + 1).enumerate() {
        let h = (3.0 + level * f64::from(max - 3)).round() as i16;
        let fade = 1.0 - age as f64 * 0.12;
        let h = ((f64::from(h) * fade).round() as i16).max(3);
        out[mid + age] = h;
        out[mid - age] = h;
    }
    out
}

/// The part of the window a mode draws on, in window coordinates. Both bars
/// sit on the same bottom line, centred.
pub fn body(mode: Mode) -> Rectangle {
    match mode {
        Mode::Resting => Rectangle {
            x: ((WIDTH - REST_WIDTH) / 2) as i16,
            y: (HEIGHT - REST_HEIGHT) as i16,
            width: REST_WIDTH,
            height: REST_HEIGHT,
        },
        _ => Rectangle {
            x: 0,
            y: 0,
            width: WIDTH,
            height: HEIGHT,
        },
    }
}

/// Commands for the bar thread.
pub enum PillCommand {
    Status(String),
    Level(f64),
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
        let x = mx + (mw as i16 - WIDTH as i16) / 2;
        let y = my + mh as i16 - HEIGHT as i16 - BOTTOM_MARGIN;

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
            WIDTH,
            HEIGHT,
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

    fn draw(&mut self, mode: Mode, heights: &[i16; BARS], phase: f64) {
        let (w, h) = (WIDTH as i16, HEIGHT as i16);
        self.fill(
            0,
            &[Rectangle {
                x: 0,
                y: 0,
                width: WIDTH,
                height: HEIGHT,
            }],
        );
        let area = body(mode);
        self.set_input(Some(area));
        match mode {
            Mode::Resting => {
                // Light outline, then the dark inside.
                self.fill_capsule(argb(0xff, 0xa8, 0xa8, 0xae), area);
                let b = REST_BORDER;
                let inner = Rectangle {
                    x: area.x + b as i16,
                    y: area.y + b as i16,
                    width: area.width - 2 * b,
                    height: area.height - 2 * b,
                };
                self.fill_capsule(argb(0xff, 0x0e, 0x0e, 0x11), inner);
            }
            Mode::Recording | Mode::Working => {
                self.fill_capsule(argb(0xf0, 0x0e, 0x0e, 0x11), area);
                let fg = argb(0xff, 0xf2, 0xf2, 0xf2);
                if mode == Mode::Recording {
                    let total = BARS as i16 * BAR_WIDTH + (BARS as i16 - 1) * BAR_GAP;
                    let left = (w - total) / 2;
                    let rects: Vec<Rectangle> = heights
                        .iter()
                        .enumerate()
                        .map(|(i, &bh)| Rectangle {
                            x: left + i as i16 * (BAR_WIDTH + BAR_GAP),
                            y: (h - bh) / 2,
                            width: BAR_WIDTH as u16,
                            height: bh as u16,
                        })
                        .collect();
                    self.fill(fg, &rects);
                } else {
                    for i in 0..3 {
                        let pulse =
                            ((phase * 6.0 - f64::from(i) * 0.9).sin() * 0.5 + 0.5) * 0.8 + 0.2;
                        let a = (pulse * 255.0) as u8;
                        let x = w / 2 - 14 + i * 12;
                        self.fill_arcs(
                            argb(a, 0xf2, 0xf2, 0xf2),
                            &[Arc {
                                x,
                                y: h / 2 - 3,
                                width: 6,
                                height: 6,
                                angle1: 0,
                                angle2: 360 * 64,
                            }],
                        );
                    }
                }
            }
            Mode::Hidden => {}
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
    let mut levels: VecDeque<f64> = VecDeque::with_capacity(16);
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
                    levels.clear();
                }
            }
            Ok(PillCommand::Level(v)) => {
                if levels.len() == 16 {
                    levels.pop_front();
                }
                levels.push_back(normalize_level(v));
            }
            Ok(PillCommand::Enabled(e)) => enabled = e,
            Ok(PillCommand::RestingBar(r)) => resting_bar = r,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        let mode = mode_for(&status, resting_bar);
        let show = enabled && mode != Mode::Hidden;
        *shown.lock().expect("mode lock poisoned") = if show { mode } else { Mode::Hidden };
        if let Some(s) = surface.as_mut() {
            s.set_visible(show);
            let animated = matches!(mode, Mode::Recording | Mode::Working);
            if show && (animated || last_drawn != Some(mode)) {
                s.draw(
                    mode,
                    &bar_heights(&levels, HEIGHT as i16 - 12),
                    start.elapsed().as_secs_f64(),
                );
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
    fn resting_bar_sits_on_the_bottom_line_centred() {
        let r = body(Mode::Resting);
        assert_eq!((r.width, r.height), (REST_WIDTH, REST_HEIGHT));
        assert_eq!(r.y as u16 + r.height, HEIGHT);
        assert_eq!(r.x as u16 * 2 + r.width, WIDTH);
        assert_eq!(body(Mode::Recording).height, HEIGHT);
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
    fn bars_are_symmetric_and_loudest_in_the_middle() {
        let levels: VecDeque<f64> = [0.2, 0.4, 1.0].into_iter().collect();
        let h = bar_heights(&levels, 24);
        let mid = BARS / 2;
        assert_eq!(h[mid], 24);
        for i in 0..BARS {
            assert_eq!(h[i], h[BARS - 1 - i]);
            assert!(h[i] >= 3);
        }
        assert!(h[mid] > h[mid + 1] && h[mid + 1] > h[mid + 2]);
        assert_eq!(h[0], 3);
    }

    #[test]
    fn colours_are_premultiplied() {
        assert_eq!(argb(0x80, 0xff, 0xff, 0xff), 0x8080_8080);
        assert_eq!(argb(0, 0xff, 0xff, 0xff), 0);
    }
}
