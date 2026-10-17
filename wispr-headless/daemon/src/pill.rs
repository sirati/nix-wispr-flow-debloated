//! The speech indicator: a small rounded bar near the bottom of the screen
//! with level bars while recording and pulsing dots while Wispr finishes.
//!
//! It is an X11 window (XWayland on GNOME and KDE): GNOME keeps no native
//! Wayland window above others, and an override-redirect X11 window is never
//! managed, focused or listed. Its input region is empty, so clicks go
//! through. If there is no X display, the indicator is simply off.

use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    Arc, ColormapAlloc, ConnectionExt as _, CreateGCAux, CreateWindowAux, Rectangle, Window,
    WindowClass,
};
use x11rb::rust_connection::RustConnection;

pub const WIDTH: u16 = 132;
pub const HEIGHT: u16 = 36;
pub const BARS: usize = 13;
const BAR_WIDTH: i16 = 4;
const BAR_GAP: i16 = 4;
const BOTTOM_MARGIN: i16 = 72;
const FRAME: Duration = Duration::from_millis(33);

/// What the indicator shows, derived from Wispr's dictation status.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    Hidden,
    Recording,
    Working,
}

pub fn mode_for(status: &str) -> Mode {
    match status {
        "recording" => Mode::Recording,
        "idle" | "testing" | "dismissed" | "" => Mode::Hidden,
        _ => Mode::Working,
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

/// Commands for the indicator thread.
pub enum PillCommand {
    Status(String),
    Level(f64),
    Enabled(bool),
}

struct Surface {
    conn: RustConnection,
    window: Window,
    gc: u32,
    mapped: bool,
}

/// Premultiplied ARGB, as a 32-bit visual expects.
fn argb(a: u8, r: u8, g: u8, b: u8) -> u32 {
    let pm = |c: u8| (u32::from(c) * u32::from(a) / 255) as u8;
    u32::from_be_bytes([a, pm(r), pm(g), pm(b)])
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
                .colormap(colormap),
        )
        .map_err(|e| e.to_string())?;
        // Clicks go through: an empty input region.
        conn.shape_rectangles(
            shape::SO::SET,
            shape::SK::INPUT,
            x11rb::protocol::xproto::ClipOrdering::UNSORTED,
            window,
            0,
            0,
            &[],
        )
        .map_err(|e| e.to_string())?;
        let gc = conn.generate_id().map_err(|e| e.to_string())?;
        conn.create_gc(gc, window, &CreateGCAux::new().foreground(0))
            .map_err(|e| e.to_string())?;
        conn.flush().map_err(|e| e.to_string())?;
        Ok(Surface {
            conn,
            window,
            gc,
            mapped: false,
        })
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
        let _ = self.conn.change_gc(
            self.gc,
            &x11rb::protocol::xproto::ChangeGCAux::new().foreground(color),
        );
        let _ = self.conn.poly_fill_rectangle(self.window, self.gc, rects);
    }

    fn fill_arcs(&self, color: u32, arcs: &[Arc]) {
        let _ = self.conn.change_gc(
            self.gc,
            &x11rb::protocol::xproto::ChangeGCAux::new().foreground(color),
        );
        let _ = self.conn.poly_fill_arc(self.window, self.gc, arcs);
    }

    fn draw(&self, mode: Mode, heights: &[i16; BARS], phase: f64) {
        let (w, h) = (WIDTH as i16, HEIGHT as i16);
        // Clear to transparent, then the rounded body.
        self.fill(
            0,
            &[Rectangle {
                x: 0,
                y: 0,
                width: WIDTH,
                height: HEIGHT,
            }],
        );
        let body = argb(0xe6, 0x14, 0x14, 0x16);
        let d = HEIGHT;
        self.fill_arcs(
            body,
            &[
                Arc {
                    x: 0,
                    y: 0,
                    width: d,
                    height: d,
                    angle1: 0,
                    angle2: 360 * 64,
                },
                Arc {
                    x: w - d as i16,
                    y: 0,
                    width: d,
                    height: d,
                    angle1: 0,
                    angle2: 360 * 64,
                },
            ],
        );
        self.fill(
            body,
            &[Rectangle {
                x: (d / 2) as i16,
                y: 0,
                width: WIDTH - d,
                height: HEIGHT,
            }],
        );

        let fg = argb(0xff, 0xf2, 0xf2, 0xf2);
        let total = BARS as i16 * BAR_WIDTH + (BARS as i16 - 1) * BAR_GAP;
        let left = (w - total) / 2;
        match mode {
            Mode::Recording => {
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
            }
            Mode::Working => {
                for i in 0..3 {
                    let pulse = ((phase * 6.0 - i as f64 * 0.9).sin() * 0.5 + 0.5) * 0.8 + 0.2;
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
            Mode::Hidden => {}
        }
        let _ = self.conn.flush();
    }
}

/// Runs the indicator until the command channel closes.
pub fn run(commands: Receiver<PillCommand>, log: impl Fn(String)) {
    let mut surface = match Surface::open() {
        Ok(s) => Some(s),
        Err(e) => {
            log(format!("speech indicator off: {e}"));
            None
        }
    };
    let mut mode = Mode::Hidden;
    let mut enabled = true;
    let mut levels: VecDeque<f64> = VecDeque::with_capacity(16);
    let start = Instant::now();
    loop {
        let timeout = if mode == Mode::Hidden {
            Duration::from_secs(3600)
        } else {
            FRAME
        };
        match commands.recv_timeout(timeout) {
            Ok(PillCommand::Status(s)) => {
                mode = mode_for(&s);
                if mode != Mode::Recording {
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
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if let Some(s) = surface.as_mut() {
            let show = enabled && mode != Mode::Hidden;
            s.set_visible(show);
            if show {
                s.draw(
                    mode,
                    &bar_heights(&levels, HEIGHT as i16 - 12),
                    start.elapsed().as_secs_f64(),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_map_to_modes() {
        assert_eq!(mode_for("recording"), Mode::Recording);
        assert_eq!(mode_for("idle"), Mode::Hidden);
        assert_eq!(mode_for("flushing"), Mode::Working);
        assert_eq!(mode_for("compressing"), Mode::Working);
        assert_eq!(mode_for("reconnecting"), Mode::Working);
        assert_eq!(mode_for("something-new"), Mode::Working);
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
