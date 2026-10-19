//! What Wispr's flow bar shows, independent of how it is drawn. This follows
//! the indicator state machine of Wispr's status page (renderer/status):
//!
//! - `status:dictationStatus` "initializing" shows the push-to-talk pill with
//!   still, dim bars; "listening" makes it push-to-talk (or keeps hands-free);
//!   "stopping", "processing" and "retrying" show the processing spinner;
//!   "idle" and "dismissed" go back to rest; "error" flashes a red outline for
//!   500 ms.
//! - `status:popo` makes the dictation hands-free: a wider pill with a cancel
//!   button (sends `status:cancelClicked`) and an accept button
//!   (`status:stopClicked`) on either side of the waveform.
//! - The bars only move after `status:micActive` true (or
//!   `status:micSilence` false) while push-to-talk or hands-free; otherwise
//!   they are 2x2 dots. See wave.rs for the movement.
//! - `status:commandModeStatus` turns outline and bars orange.
//!
//! Anything else the page shows (polish results, instruct mode, reminders,
//! notifications) loads Wispr's own page instead; the shim decides that.
//!
//! The output is a list of shapes in Wispr's CSS pixels relative to the
//! pill's top left corner, y down; pill_x11.rs (Linux) and pill_mac.rs (macOS)
//! draw them. The pill is red instead of Wispr's black, so it is clear on
//! screen whether Rust or Wispr's page draws it.

use std::time::{Duration, Instant};

use crate::wave;

/// Gap between the bottom of the pill and the bottom of the screen.
pub const BOTTOM_MARGIN: f64 = 10.0;
pub const FRAME: Duration = Duration::from_millis(16);
/// The largest pill (hands-free), for a fixed-size window.
pub const MAX_SIZE: (f64, f64) = (102.5, 30.0);

/// Pill fill: red (Wispr: black, opaque while dictating, 50 % at rest).
pub const RED: [u8; 3] = [0xc8, 0x14, 0x14];
const REST_OUTLINE: [u8; 4] = [0xaa, 0xaa, 0xaa, 0xc0];
const OUTLINE: [u8; 4] = [0x30, 0x30, 0x2f, 0xff];
const COMMAND: [u8; 3] = [0xff, 0xa9, 0x46];
const ERROR_OUTLINE: [u8; 4] = [0xee, 0x6a, 0x6a, 0xff];
const CANCEL_BG: [u8; 4] = [0x4d, 0x4a, 0x42, 0xff];
const CANCEL_FG: [u8; 4] = [0xfc, 0xfc, 0xfb, 0xff];
const ACCEPT_BG: [u8; 4] = [0xfc, 0xfc, 0xfb, 0xff];
const ACCEPT_FG: [u8; 4] = [0x1a, 0x1a, 0x1a, 0xff];
const SPINNER: [u8; 3] = [0xa3, 0x94, 0xc7];
const ERROR_TIME: Duration = Duration::from_millis(500);

/// Waveform box: 10 bars of 2 px with 2 px gaps, 4 px padding each side.
const WAVE_BOX: f64 = 46.0;
const WAVE_PAD: f64 = 4.0;
const BAR_GAP: f64 = 2.0;
const BUTTON: f64 = 18.0;
const GAP: f64 = 4.0;
const SPINNER_BOX: f64 = 16.0;
const SPINNER_GAP: f64 = 6.0;

/// What the bar shows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    Hidden,
    Resting,
    /// Push-to-talk pill before the recording runs.
    Initializing,
    PushToTalk,
    HandsFree,
    Processing,
    Error,
}

impl Mode {
    /// Pill size, CSS pixels (Wispr's horizontal layout).
    pub fn size(self) -> (f64, f64) {
        match self {
            Mode::Hidden | Mode::Resting => (40.0, 8.0),
            Mode::Initializing | Mode::PushToTalk => (73.0, 30.0),
            Mode::HandsFree => MAX_SIZE,
            Mode::Processing => (98.0, 30.0),
            Mode::Error => (91.0, 30.0),
        }
    }
}

/// The page's indicator state (only what we draw).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Indicator {
    Rest,
    Initializing,
    PushToTalk,
    HandsFree,
    Processing,
    Error(Instant),
}

/// What a click at `(x, y)` (CSS pixels in the pill) means in each mode.
pub fn click_action(mode: Mode, x: f64, _y: f64) -> Option<&'static str> {
    match mode {
        Mode::Resting => Some("start"),
        Mode::PushToTalk => Some("stop"),
        Mode::HandsFree => {
            let (cancel, accept) = buttons();
            if x <= cancel + BUTTON / 2.0 + GAP / 2.0 {
                Some("cancel")
            } else if x >= accept - BUTTON / 2.0 - GAP / 2.0 {
                Some("stop")
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Centre x of the hands-free cancel and accept buttons.
fn buttons() -> (f64, f64) {
    let content = BUTTON + GAP + WAVE_BOX + GAP + BUTTON;
    let left = (MAX_SIZE.0 - content) / 2.0;
    (left + BUTTON / 2.0, left + content - BUTTON / 2.0)
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

/// Alpha of spinner bar `i` (0..8) at `t` seconds: each fades from .88 to .15
/// over 1.1 s, bar i starting (7 - i) x 0.1375 s into the loop.
pub fn spinner_alpha(t: f64, i: usize) -> f64 {
    let p = (t + (7 - i) as f64 * 0.1375) / 1.1;
    0.88 - 0.73 * (p - p.floor())
}

/// Commands for the bar.
pub enum PillCommand {
    Status(String),
    Popo,
    Level(f64),
    /// `status:micActive`.
    Mic(bool),
    /// `status:micSilence`.
    Silence(bool),
    CommandMode(bool),
    /// `status:setIndicatorState`.
    Indicator(String),
    /// Wispr's own status page is up and draws the bar itself.
    Enabled(bool),
    /// Whether Wispr shows its resting bar (its hideFlowBarPermanently and
    /// snooze preferences).
    RestingBar(bool),
}

/// A rectangle with rounded corners, rotated about its centre; CSS pixels,
/// y down, angle in radians clockwise.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shape {
    pub cx: f64,
    pub cy: f64,
    pub w: f64,
    pub h: f64,
    pub angle: f64,
    pub radius: f64,
    pub fill: [u8; 4],
    /// Outline colour and width, drawn inside the shape.
    pub border: Option<([u8; 4], f64)>,
}

impl Shape {
    fn rect(x: f64, y: f64, w: f64, h: f64, radius: f64, fill: [u8; 4]) -> Shape {
        Shape {
            cx: x + w / 2.0,
            cy: y + h / 2.0,
            w,
            h,
            angle: 0.0,
            radius,
            fill,
            border: None,
        }
    }

    /// A stroke from `a` to `b` with round ends.
    fn line(a: (f64, f64), b: (f64, f64), width: f64, fill: [u8; 4]) -> Shape {
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        Shape {
            cx: (a.0 + b.0) / 2.0,
            cy: (a.1 + b.1) / 2.0,
            w: (dx * dx + dy * dy).sqrt() + width,
            h: width,
            angle: dy.atan2(dx),
            radius: width / 2.0,
            fill,
            border: None,
        }
    }
}

/// One frame to draw.
pub struct Frame {
    pub mode: Mode,
    /// Pill size; shapes are relative to its top left.
    pub size: (f64, f64),
    /// The pill first, then everything on it.
    pub shapes: Vec<Shape>,
    /// Whether the next frame differs (animation running).
    pub animated: bool,
}

/// The bar's state, fed with commands; the drawing code asks it what to show.
pub struct BarState {
    indicator: Indicator,
    resting_bar: bool,
    enabled: bool,
    mic_active: bool,
    waving: bool,
    command_mode: bool,
    target: f64,
    level: f64,
    last_frame: Instant,
    start: Instant,
}

impl Default for BarState {
    fn default() -> BarState {
        let now = Instant::now();
        BarState {
            indicator: Indicator::Rest,
            resting_bar: false,
            enabled: true,
            mic_active: false,
            waving: false,
            command_mode: false,
            target: 0.0,
            level: 0.0,
            last_frame: now,
            start: now,
        }
    }
}

impl BarState {
    fn recording(&self) -> bool {
        matches!(self.indicator, Indicator::PushToTalk | Indicator::HandsFree)
    }

    fn rest(&mut self) {
        self.indicator = Indicator::Rest;
        self.command_mode = false;
        self.mic_active = false;
        self.waving = false;
    }

    pub fn apply(&mut self, command: PillCommand) {
        match command {
            PillCommand::Status(s) => match s.as_str() {
                "initializing" => {
                    self.indicator = Indicator::Initializing;
                    self.waving = false;
                }
                "listening" => {
                    if self.indicator != Indicator::HandsFree {
                        self.indicator = Indicator::PushToTalk;
                    }
                    self.mic_active = false;
                }
                "stopping" | "processing" | "retrying" => {
                    self.indicator = Indicator::Processing;
                    self.mic_active = false;
                    self.waving = false;
                }
                "idle" | "dismissed" => self.rest(),
                "error" => self.indicator = Indicator::Error(Instant::now() + ERROR_TIME),
                _ => {}
            },
            PillCommand::Popo => self.indicator = Indicator::HandsFree,
            PillCommand::Level(v) => {
                if self.recording() {
                    self.target = normalize_level(v);
                }
            }
            PillCommand::Mic(on) => {
                if self.recording() {
                    self.mic_active = on;
                    self.waving = on;
                }
            }
            PillCommand::Silence(silent) => {
                if silent {
                    self.mic_active = false;
                    self.waving = false;
                } else if self.recording() {
                    self.mic_active = true;
                    self.waving = true;
                }
            }
            PillCommand::CommandMode(on) => self.command_mode = on,
            PillCommand::Indicator(state) => {
                if state == "resting" {
                    self.rest();
                }
            }
            PillCommand::Enabled(e) => self.enabled = e,
            PillCommand::RestingBar(r) => self.resting_bar = r,
        }
        if !self.recording() {
            self.target = 0.0;
        }
    }

    /// The mode on screen: Hidden while Wispr's own page draws the bar.
    pub fn shown(&self, now: Instant) -> Mode {
        if !self.enabled {
            return Mode::Hidden;
        }
        match self.indicator {
            Indicator::Rest if self.resting_bar => Mode::Resting,
            Indicator::Rest => Mode::Hidden,
            Indicator::Error(until) if now >= until => {
                if self.resting_bar {
                    Mode::Resting
                } else {
                    Mode::Hidden
                }
            }
            Indicator::Error(_) => Mode::Error,
            Indicator::Initializing => Mode::Initializing,
            Indicator::PushToTalk => Mode::PushToTalk,
            Indicator::HandsFree => Mode::HandsFree,
            Indicator::Processing => Mode::Processing,
        }
    }

    /// Advances the animations to `now` and returns what to draw.
    pub fn frame(&mut self, now: Instant) -> Frame {
        if let Indicator::Error(until) = self.indicator {
            if now >= until {
                self.rest();
            }
        }
        let dt = now.duration_since(self.last_frame).as_secs_f64() * 1000.0;
        self.level = wave::smooth(self.level, self.target, dt);
        self.last_frame = now;
        let t = now.duration_since(self.start).as_secs_f64();
        let mode = self.shown(now);
        let size = mode.size();
        let mut shapes = Vec::new();
        let animated = match mode {
            Mode::Hidden => false,
            Mode::Resting => {
                shapes.push(pill(
                    size,
                    [RED[0], RED[1], RED[2], 0x80],
                    REST_OUTLINE,
                    1.0,
                ));
                false
            }
            _ => {
                let outline = match mode {
                    Mode::Error => (ERROR_OUTLINE, 1.5),
                    _ if self.command_mode => ([COMMAND[0], COMMAND[1], COMMAND[2], 0xff], 1.0),
                    _ => (OUTLINE, 1.0),
                };
                shapes.push(pill(
                    size,
                    [RED[0], RED[1], RED[2], 0xff],
                    outline.0,
                    outline.1,
                ));
                self.content(mode, size, t, &mut shapes)
            }
        };
        Frame {
            mode,
            size,
            shapes,
            animated,
        }
    }

    /// The pill's content; returns whether it is animated.
    fn content(&self, mode: Mode, size: (f64, f64), t: f64, shapes: &mut Vec<Shape>) -> bool {
        let cy = size.1 / 2.0;
        let wave_left = match mode {
            Mode::HandsFree => {
                let (cancel, accept) = buttons();
                cancel_button(cancel, cy, shapes);
                accept_button(accept, cy, shapes);
                cancel + BUTTON / 2.0 + GAP
            }
            Mode::Processing => {
                let left = (size.0 - (SPINNER_BOX + SPINNER_GAP + WAVE_BOX)) / 2.0;
                spinner(left + SPINNER_BOX / 2.0, cy, t, self.command_mode, shapes);
                left + SPINNER_BOX + SPINNER_GAP
            }
            Mode::Error => return true,
            _ => (size.0 - WAVE_BOX) / 2.0,
        };
        let waving = self.waving && matches!(mode, Mode::PushToTalk | Mode::HandsFree);
        let heights = if waving {
            wave::heights(t, self.level)
        } else {
            [wave::BAR_BASE; wave::BARS]
        };
        let alpha = if self.mic_active { 0xff } else { 0x66 };
        let [r, g, b] = if self.command_mode {
            COMMAND
        } else {
            [0xff; 3]
        };
        for (i, h) in heights.iter().enumerate() {
            let x = wave_left + WAVE_PAD + i as f64 * (wave::BAR_WIDTH + BAR_GAP);
            let h = h.max(wave::BAR_BASE);
            shapes.push(Shape::rect(
                x,
                cy - h / 2.0,
                wave::BAR_WIDTH,
                h,
                0.0,
                [r, g, b, alpha],
            ));
        }
        waving || mode == Mode::Processing
    }
}

fn pill(size: (f64, f64), fill: [u8; 4], outline: [u8; 4], width: f64) -> Shape {
    let mut s = Shape::rect(0.0, 0.0, size.0, size.1, size.1 / 2.0, fill);
    s.border = Some((outline, width));
    s
}

/// Lucide's 12 px icons (24-unit viewBox, stroke 3) scaled to CSS pixels.
const ICON: f64 = 12.0 / 24.0;

fn icon_line(cx: f64, cy: f64, a: (f64, f64), b: (f64, f64), fill: [u8; 4]) -> Shape {
    let p = |(x, y): (f64, f64)| (cx + (x - 12.0) * ICON, cy + (y - 12.0) * ICON);
    Shape::line(p(a), p(b), 3.0 * ICON, fill)
}

fn cancel_button(cx: f64, cy: f64, shapes: &mut Vec<Shape>) {
    let r = BUTTON / 2.0;
    shapes.push(Shape::rect(cx - r, cy - r, BUTTON, BUTTON, r, CANCEL_BG));
    // lucide x: M18 6 6 18, m6 6 12 12
    shapes.push(icon_line(cx, cy, (18.0, 6.0), (6.0, 18.0), CANCEL_FG));
    shapes.push(icon_line(cx, cy, (6.0, 6.0), (18.0, 18.0), CANCEL_FG));
}

fn accept_button(cx: f64, cy: f64, shapes: &mut Vec<Shape>) {
    let r = BUTTON / 2.0;
    shapes.push(Shape::rect(cx - r, cy - r, BUTTON, BUTTON, r, ACCEPT_BG));
    // lucide check: M20 6 9 17l-5-5
    shapes.push(icon_line(cx, cy, (20.0, 6.0), (9.0, 17.0), ACCEPT_FG));
    shapes.push(icon_line(cx, cy, (9.0, 17.0), (4.0, 12.0), ACCEPT_FG));
}

/// Wispr's processing spinner: 8 bars of 2x5 px, 6 px from the centre.
fn spinner(cx: f64, cy: f64, t: f64, command_mode: bool, shapes: &mut Vec<Shape>) {
    let [r, g, b] = if command_mode { COMMAND } else { SPINNER };
    for i in 0..8 {
        let angle = (i as f64 * 45.0).to_radians();
        let alpha = (spinner_alpha(t, i) * 255.0).round() as u8;
        shapes.push(Shape {
            cx: cx + 6.0 * angle.sin(),
            cy: cy - 6.0 * angle.cos(),
            w: 2.0,
            h: 5.0,
            angle,
            radius: 1.0,
            fill: [r, g, b, alpha],
            border: None,
        });
    }
}

/// `top` composited over `bottom` (both straight alpha), for drawing code
/// that cannot blend.
pub fn over(top: [u8; 4], bottom: [u8; 4]) -> [u8; 4] {
    let (ta, ba) = (f64::from(top[3]) / 255.0, f64::from(bottom[3]) / 255.0);
    let a = ta + ba * (1.0 - ta);
    if a <= 0.0 {
        return [0; 4];
    }
    let mix =
        |t: u8, b: u8| ((f64::from(t) * ta + f64::from(b) * ba * (1.0 - ta)) / a).round() as u8;
    [
        mix(top[0], bottom[0]),
        mix(top[1], bottom[1]),
        mix(top[2], bottom[2]),
        (a * 255.0).round() as u8,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> BarState {
        let mut s = BarState::default();
        s.apply(PillCommand::RestingBar(true));
        s
    }

    fn now() -> Instant {
        Instant::now()
    }

    #[test]
    fn push_to_talk_goes_through_wispr_states() {
        let mut s = state();
        assert_eq!(s.shown(now()), Mode::Resting);
        s.apply(PillCommand::Status("initializing".into()));
        assert_eq!(s.shown(now()), Mode::Initializing);
        s.apply(PillCommand::Status("listening".into()));
        assert_eq!(s.shown(now()), Mode::PushToTalk);
        s.apply(PillCommand::Status("stopping".into()));
        assert_eq!(s.shown(now()), Mode::Processing);
        s.apply(PillCommand::Status("idle".into()));
        assert_eq!(s.shown(now()), Mode::Resting);
    }

    #[test]
    fn bars_move_only_once_the_mic_is_active_while_recording() {
        let mut s = state();
        s.apply(PillCommand::Status("initializing".into()));
        s.apply(PillCommand::Mic(true));
        assert!(
            !s.frame(now()).animated,
            "dropped while initializing, as in Wispr"
        );
        s.apply(PillCommand::Status("listening".into()));
        assert!(!s.frame(now()).animated, "still dots until the first audio");
        s.apply(PillCommand::Mic(true));
        s.apply(PillCommand::Level(1.0));
        let f = s.frame(now() + Duration::from_secs(1));
        assert!(f.animated);
        let tallest = f.shapes[1..].iter().map(|b| b.h).fold(0.0, f64::max);
        assert!(
            tallest > 2.0 * wave::BAR_BASE,
            "a loud voice makes tall bars"
        );
        s.apply(PillCommand::Silence(true));
        assert!(!s.frame(now()).animated);
    }

    #[test]
    fn hands_free_keeps_its_pill_through_listening() {
        let mut s = state();
        s.apply(PillCommand::Status("initializing".into()));
        s.apply(PillCommand::Popo);
        s.apply(PillCommand::Status("listening".into()));
        assert_eq!(s.shown(now()), Mode::HandsFree);
        assert_eq!(Mode::HandsFree.size(), (102.5, 30.0));
    }

    #[test]
    fn hands_free_buttons_cancel_and_accept() {
        let (cancel, accept) = buttons();
        assert_eq!(click_action(Mode::HandsFree, cancel, 15.0), Some("cancel"));
        assert_eq!(click_action(Mode::HandsFree, accept, 15.0), Some("stop"));
        assert_eq!(click_action(Mode::HandsFree, MAX_SIZE.0 / 2.0, 15.0), None);
        assert_eq!(click_action(Mode::PushToTalk, 10.0, 15.0), Some("stop"));
        assert_eq!(click_action(Mode::Resting, 10.0, 4.0), Some("start"));
        assert_eq!(click_action(Mode::Processing, 10.0, 15.0), None);
        // [cancel 18] 4 [waveform 46] 4 [accept 18], centred.
        assert!((accept - cancel - (18.0 + 4.0 + 46.0 + 4.0)).abs() < 1e-9);
        assert!((cancel - 9.0 - (MAX_SIZE.0 - 90.0) / 2.0).abs() < 1e-9);
    }

    #[test]
    fn errors_flash_then_rest() {
        let mut s = state();
        s.apply(PillCommand::Status("error".into()));
        assert_eq!(s.shown(now()), Mode::Error);
        let later = now() + Duration::from_millis(600);
        assert_eq!(s.frame(later).mode, Mode::Resting);
    }

    #[test]
    fn wispr_page_hides_the_bar() {
        let mut s = state();
        s.apply(PillCommand::Enabled(false));
        assert_eq!(s.shown(now()), Mode::Hidden);
        assert!(s.frame(now()).shapes.is_empty());
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
    fn spinner_fades_within_range() {
        for i in 0..8 {
            for k in 0..200 {
                let a = spinner_alpha(k as f64 * 0.013, i);
                assert!((0.15..=0.88).contains(&a), "{a}");
            }
        }
    }

    #[test]
    fn compositing_matches_source_over() {
        assert_eq!(
            over([255, 255, 255, 255], [200, 0, 0, 255]),
            [255, 255, 255, 255]
        );
        assert_eq!(over([0, 0, 0, 0], [200, 0, 0, 255]), [200, 0, 0, 255]);
        let half = over([255, 255, 255, 0x66], [200, 20, 20, 255]);
        assert_eq!(half[3], 255);
        assert!(half[0] > 200 && half[1] > 20);
    }
}
