//! The flow bar on Linux (see bar.rs for what it shows).
//!
//! It is an X11 window (XWayland on GNOME and KDE): GNOME keeps no native
//! Wayland window above others, and an override-redirect X11 window is never
//! managed, focused or listed. Its input region is the drawn pill only, so
//! clicks next to it go through. A separate thread waits for X events, so a
//! resting bar costs no wakeups. If there is no X display, the bar is off.
//!
//! Sizes are multiplied by the display scale (Xft.dpi / 96, so 2 on a 200 %
//! screen). X's core drawing writes pixels without blending, so shapes on the
//! pill are composited over its colour first.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc as Shared, Mutex};
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    Arc, AtomEnum, ChangeGCAux, ClipOrdering, ColormapAlloc, ConfigureWindowAux,
    ConnectionExt as _, CoordMode, CreateGCAux, CreateWindowAux, EventMask, ImageFormat,
    ImageOrder, Point, PolyShape, Rectangle, Window, WindowClass,
};
use x11rb::protocol::Event as XEvent;
use x11rb::rust_connection::RustConnection;

use crate::bar::{
    over, BarEvent, BarState, Frame, Mode, PillCommand, Shape, Text, BOTTOM_MARGIN, FRAME,
    WINDOW_SIZE,
};
use crate::text::Fonts;

/// Display scale from an X resource string (`Xft.dpi: 192` gives 2).
pub fn scale_from_resources(resources: &str) -> f64 {
    resources
        .lines()
        .find_map(|l| l.strip_prefix("Xft.dpi:"))
        .and_then(|v| v.trim().parse::<f64>().ok())
        .map(|dpi| (dpi / 96.0).clamp(1.0, 4.0))
        .unwrap_or(1.0)
}

/// Device-pixel geometry for one display scale: a window as large as the
/// largest pill, each pill centred on its bottom line.
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
            width: px(WINDOW_SIZE.0, scale),
            height: px(WINDOW_SIZE.1, scale),
        }
    }

    /// A pill of `(w, h)` CSS pixels, in window pixels.
    pub fn body(&self, (w, h): (f64, f64)) -> Rectangle {
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
    /// Frames are drawn here, then copied to the window in one go (no flicker).
    buffer: u32,
    gc: u32,
    mapped: bool,
    input: Option<(i16, i16, u16, u16)>,
    geo: Geometry,
    fonts: Option<Fonts>,
    /// The server wants image pixels least significant byte first.
    lsb: bool,
}

/// Premultiplied ARGB, as a 32-bit visual expects.
fn argb([r, g, b, a]: [u8; 4]) -> u32 {
    let pm = |c: u8| (u32::from(c) * u32::from(a) / 255) as u8;
    u32::from_be_bytes([a, pm(r), pm(g), pm(b)])
}

/// A rounded bar along its long side: two half circles and the rectangle
/// between them (a circle when square).
fn capsule(r: Rectangle) -> (Vec<Arc>, Rectangle) {
    let d = r.width.min(r.height);
    let circle = |x: i16, y: i16| Arc {
        x,
        y,
        width: d,
        height: d,
        angle1: 0,
        angle2: 360 * 64,
    };
    if r.width >= r.height {
        let arcs = vec![circle(r.x, r.y), circle(r.x + (r.width - d) as i16, r.y)];
        let mid = Rectangle {
            x: r.x + (d / 2) as i16,
            y: r.y,
            width: r.width - d,
            height: r.height,
        };
        (arcs, mid)
    } else {
        let arcs = vec![circle(r.x, r.y), circle(r.x, r.y + (r.height - d) as i16)];
        let mid = Rectangle {
            x: r.x,
            y: r.y + (d / 2) as i16,
            width: r.width,
            height: r.height - d,
        };
        (arcs, mid)
    }
}

/// A rectangle with corners of radius `rad`: four quarter-circles (drawn as
/// full circles, covered by the bands) and two crossing bands.
fn rounded(r: Rectangle, rad: u16) -> (Vec<Arc>, Vec<Rectangle>) {
    let rad = rad.min(r.width / 2).min(r.height / 2);
    let d = 2 * rad;
    let (right, bottom) = (r.x + (r.width - d) as i16, r.y + (r.height - d) as i16);
    let arcs = [(r.x, r.y), (right, r.y), (r.x, bottom), (right, bottom)]
        .map(|(x, y)| Arc {
            x,
            y,
            width: d,
            height: d,
            angle1: 0,
            angle2: 360 * 64,
        })
        .to_vec();
    let rects = vec![
        Rectangle {
            x: r.x + rad as i16,
            y: r.y,
            width: r.width - d,
            height: r.height,
        },
        Rectangle {
            x: r.x,
            y: r.y + rad as i16,
            width: r.width,
            height: r.height - d,
        },
    ];
    (arcs, rects)
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
    fn open(fonts: Option<Fonts>) -> Result<Surface, String> {
        let (conn, screen_num) = x11rb::connect(None).map_err(|e| format!("no X display: {e}"))?;
        let screen = conn.setup().roots[screen_num].clone();
        let lsb = conn.setup().image_byte_order == ImageOrder::LSB_FIRST;
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
        let buffer = conn.generate_id().map_err(|e| e.to_string())?;
        conn.create_pixmap(32, buffer, window, geo.width, geo.height)
            .map_err(|e| e.to_string())?;
        conn.flush().map_err(|e| e.to_string())?;
        let mut s = Surface {
            conn: Shared::new(conn),
            window,
            buffer,
            gc,
            mapped: false,
            input: None,
            geo,
            fonts,
            lsb,
        };
        s.set_input(None);
        Ok(s)
    }

    /// Clicks are taken only on the drawn pill.
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

    fn color(&self, color: [u8; 4]) {
        let _ = self
            .conn
            .change_gc(self.gc, &ChangeGCAux::new().foreground(argb(color)));
    }

    /// An unrotated shape filled with `color`, inset by `inset` pixels.
    fn fill_box(&self, s: &Shape, origin: (f64, f64), inset: f64, color: [u8; 4]) {
        let k = self.geo.scale;
        let x = origin.0 + (s.cx - s.w / 2.0) * k + inset;
        let y = origin.1 + (s.cy - s.h / 2.0) * k + inset;
        let r = Rectangle {
            x: x.round() as i16,
            y: y.round() as i16,
            width: (s.w * k - 2.0 * inset).round().max(1.0) as u16,
            height: (s.h * k - 2.0 * inset).round().max(1.0) as u16,
        };
        self.color(color);
        if s.radius * 2.0 >= s.w.min(s.h) - 0.01 {
            let (arcs, mid) = capsule(r);
            let _ = self.conn.poly_fill_arc(self.buffer, self.gc, &arcs);
            let _ = self.conn.poly_fill_rectangle(self.buffer, self.gc, &[mid]);
        } else if s.radius * k >= 1.0 {
            let (arcs, rects) = rounded(r, (s.radius * k).round() as u16);
            let _ = self.conn.poly_fill_arc(self.buffer, self.gc, &arcs);
            let _ = self.conn.poly_fill_rectangle(self.buffer, self.gc, &rects);
        } else {
            let _ = self.conn.poly_fill_rectangle(self.buffer, self.gc, &[r]);
        }
    }

    /// A rotated shape as a polygon (its rounding is too small to matter).
    fn fill_rotated(&self, s: &Shape, origin: (f64, f64), color: [u8; 4]) {
        let k = self.geo.scale;
        let (sin, cos) = s.angle.sin_cos();
        let (hw, hh) = (s.w / 2.0, s.h / 2.0);
        let points: Vec<Point> = [(-hw, -hh), (hw, -hh), (hw, hh), (-hw, hh)]
            .iter()
            .map(|&(dx, dy)| {
                let x = s.cx + dx * cos - dy * sin;
                let y = s.cy + dx * sin + dy * cos;
                Point {
                    x: (origin.0 + x * k).round() as i16,
                    y: (origin.1 + y * k).round() as i16,
                }
            })
            .collect();
        self.color(color);
        let _ = self.conn.fill_poly(
            self.buffer,
            self.gc,
            PolyShape::CONVEX,
            CoordMode::ORIGIN,
            &points,
        );
    }

    fn draw(&mut self, f: &Frame) {
        let height = px(f.size.1.max(WINDOW_SIZE.1), self.geo.scale);
        if height != self.geo.height {
            if let Ok(cookie) = self.conn.get_geometry(self.window) {
                if let Ok(old) = cookie.reply() {
                    let _ = self.conn.configure_window(
                        self.window,
                        &ConfigureWindowAux::new()
                            .height(u32::from(height))
                            .y(i32::from(old.y) + i32::from(self.geo.height) - i32::from(height)),
                    );
                    let _ = self.conn.free_pixmap(self.buffer);
                    let _ = self.conn.create_pixmap(
                        32,
                        self.buffer,
                        self.window,
                        self.geo.width,
                        height,
                    );
                    self.geo.height = height;
                    self.input = None;
                }
            }
        }
        let g = self.geo;
        self.color([0; 4]);
        let _ = self.conn.poly_fill_rectangle(
            self.buffer,
            self.gc,
            &[Rectangle {
                x: 0,
                y: 0,
                width: g.width,
                height: g.height,
            }],
        );
        let area = g.body(f.size);
        self.set_input(Some(area));
        let origin = (f64::from(area.x), f64::from(area.y));
        let pill = f.shapes.first().map(|s| s.fill).unwrap_or([0; 4]);
        for (i, s) in f.shapes.iter().enumerate() {
            let fill = if i == 0 { s.fill } else { over(s.fill, pill) };
            if let Some((outline, width)) = s.border {
                self.fill_box(s, origin, 0.0, outline);
                self.fill_box(s, origin, (width * g.scale).round().max(1.0), fill);
            } else if s.angle.abs() < 1e-6 {
                self.fill_box(s, origin, 0.0, fill);
            } else {
                self.fill_rotated(s, origin, fill);
            }
        }
        for placed in &f.icons {
            let icon = &placed.icon;
            let pixels = image::imageops::resize(
                &icon.pixels,
                px(18.0, g.scale) as u32,
                px(18.0, g.scale) as u32,
                image::imageops::FilterType::Lanczos3,
            );
            let data: Vec<u8> = pixels
                .pixels()
                .flat_map(|p| {
                    let value = argb(over(p.0, pill));
                    if self.lsb {
                        value.to_le_bytes()
                    } else {
                        value.to_be_bytes()
                    }
                })
                .collect();
            let _ = self.conn.put_image(
                ImageFormat::Z_PIXMAP,
                self.buffer,
                self.gc,
                pixels.width() as u16,
                pixels.height() as u16,
                (origin.0 + placed.x * g.scale).round() as i16,
                (origin.1 + placed.y * g.scale).round() as i16,
                0,
                32,
                &data,
            );
        }
        for t in &f.texts {
            self.draw_text(t, origin);
        }
        let _ = self.conn.copy_area(
            self.buffer,
            self.window,
            self.gc,
            0,
            0,
            0,
            0,
            g.width,
            g.height,
        );
        let _ = self.conn.flush();
    }

    fn draw_text(&self, t: &Text, origin: (f64, f64)) {
        let Some(fonts) = &self.fonts else { return };
        let k = self.geo.scale;
        let (w, h, pixels) = fonts.render(t, k);
        let data: Vec<u8> = pixels
            .iter()
            .flat_map(|&p| {
                let v = argb(p);
                if self.lsb {
                    v.to_le_bytes()
                } else {
                    v.to_be_bytes()
                }
            })
            .collect();
        let _ = self.conn.put_image(
            ImageFormat::Z_PIXMAP,
            self.buffer,
            self.gc,
            w,
            h,
            (origin.0 + t.x * k).round() as i16,
            (origin.1 + t.top * k).round() as i16,
            0,
            32,
            &data,
        );
    }

    /// The pointer in CSS pixels of the pill `area`, if it is on it.
    fn pointer(&self, area: Rectangle) -> Option<(f64, f64)> {
        let p = self.conn.query_pointer(self.window).ok()?.reply().ok()?;
        let (x, y) = (p.win_x - area.x, p.win_y - area.y);
        let inside = p.same_screen
            && x >= 0
            && y >= 0
            && (x as u16) < area.width
            && (y as u16) < area.height;
        inside.then(|| (f64::from(x) / self.geo.scale, f64::from(y) / self.geo.scale))
    }
}

/// What the bar's loop waits for.
enum Input {
    Command(PillCommand),
    /// A left click, in CSS pixels of the pill on screen.
    Click(f64, f64),
    Quit,
}

/// Runs the bar until the command channel closes. `fonts` is a directory
/// with Figtree; `on_event` gets clicks and notification results.
pub fn run(
    commands: Receiver<PillCommand>,
    fonts: Option<&str>,
    log: impl Fn(String),
    on_event: impl Fn(BarEvent),
) {
    let fonts = fonts.and_then(|dir| {
        Fonts::load(dir)
            .map_err(|e| log(format!("no text in the flow bar: {e}")))
            .ok()
    });
    let fallback_width = |text: &str, size: f64| 0.55 * size * text.chars().count() as f64;
    let mut surface = match Surface::open(fonts) {
        Ok(s) => Some(s),
        Err(e) => {
            log(format!("flow bar off: {e}"));
            None
        }
    };

    // One queue for commands and clicks.
    let (tx, inputs) = mpsc::channel::<Input>();
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for c in commands {
                if tx.send(Input::Command(c)).is_err() {
                    return;
                }
            }
            let _ = tx.send(Input::Quit);
        });
    }
    // Clicks: a thread that blocks on X events, converted to pill pixels with
    // the pill on screen.
    let shown_area = Shared::new(Mutex::new(Rectangle::default()));
    if let Some(s) = &surface {
        let conn = s.conn.clone();
        let scale = s.geo.scale;
        let area = shown_area.clone();
        std::thread::spawn(move || {
            while let Ok(event) = conn.wait_for_event() {
                if let XEvent::ButtonPress(press) = event {
                    if press.detail != 1 {
                        continue;
                    }
                    let a = *area.lock().expect("area lock poisoned");
                    let x = f64::from(press.event_x - a.x) / scale;
                    let y = f64::from(press.event_y - a.y) / scale;
                    if tx.send(Input::Click(x, y)).is_err() {
                        return;
                    }
                }
            }
        });
    }

    let mut state = BarState::default();
    let mut last: Option<(Mode, Rectangle)> = None;
    let mut animated = false;
    loop {
        // Only animations need frames; a resting or hidden bar sleeps.
        let timeout = if animated {
            FRAME
        } else {
            Duration::from_secs(3600)
        };
        match inputs.recv_timeout(timeout) {
            Ok(Input::Command(c)) => state.apply(c),
            Ok(Input::Click(x, y)) => {
                if let Some(e) = state.click(x, y) {
                    on_event(e);
                }
            }
            Ok(Input::Quit) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }
        let pointer = match (&surface, last) {
            (Some(s), Some((Mode::Notice, area))) => s.pointer(area),
            _ => None,
        };
        let f = match surface.as_ref().and_then(|s| s.fonts.as_ref()) {
            Some(fonts) => state.frame(
                Instant::now(),
                &|t, size, bold| fonts.width(t, size, bold),
                pointer,
            ),
            None => state.frame(
                Instant::now(),
                &|t, size, _| fallback_width(t, size),
                pointer,
            ),
        };
        for e in &f.events {
            on_event(*e);
        }
        let mut area = Rectangle::default();
        if let Some(s) = surface.as_mut() {
            let show = f.mode != Mode::Hidden;
            s.set_visible(show);
            if show {
                s.draw(&f);
            }
            area = s.geo.body(f.size);
            *shown_area.lock().expect("area lock poisoned") = area;
        }
        last = Some((f.mode, area));
        animated = f.animated;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resting_bar_matches_wispr_at_200_percent() {
        let g = Geometry::new(2.0);
        let r = g.body((40.0, 8.0));
        assert_eq!((r.width, r.height), (80, 16), "measured from Wispr's page");
        assert_eq!(r.y as u16 + r.height, g.height, "sits on the bottom line");
        assert!((r.x as u16 * 2 + r.width).abs_diff(g.width) <= 1, "centred");
        let notice = g.body(WINDOW_SIZE);
        assert_eq!((notice.width, notice.height), (g.width, g.height));
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
        assert_eq!(argb([0xff, 0xff, 0xff, 0x80]), 0x8080_8080);
        assert_eq!(argb([0xff, 0xff, 0xff, 0]), 0);
    }

    #[test]
    fn capsules_follow_their_long_side() {
        let r = Rectangle {
            x: 0,
            y: 0,
            width: 4,
            height: 10,
        };
        let (arcs, mid) = capsule(r);
        assert_eq!(arcs[1].y, 6);
        assert_eq!((mid.y, mid.height), (2, 6));
    }
}
