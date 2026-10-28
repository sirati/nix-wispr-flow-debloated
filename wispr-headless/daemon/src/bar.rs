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
//! - Simple notifications (Wispr's "condensed" layout, such as "Transcript
//!   cancelled" with Undo) replace the bar: a title, up to two buttons and a
//!   progress bar. As on Wispr's page, the timer runs here (paused while the
//!   pointer is on it), and a clicked button shows a check for 600 ms.
//!
//! Anything else the page shows (polish results, instruct mode, reminders,
//! other notifications) loads Wispr's own page instead; the shim decides that.
//!
//! The output is a list of shapes and texts in Wispr's CSS pixels relative to
//! the pill's top left corner, y down; pill_x11.rs (Linux) and pill_mac.rs
//! (macOS) draw them. The pill is Wispr's black unless --bar-color says
//! otherwise (a red one makes it clear on screen whether Rust or Wispr's page
//! draws it); while a dictation goes to the local model it has a colour of
//! its own, green unless --local-bar-color says otherwise.

use std::time::{Duration, Instant};

use crate::wave;

/// Gap between the bottom of the pill and the bottom of the screen.
pub const BOTTOM_MARGIN: f64 = 10.0;
pub const FRAME: Duration = Duration::from_millis(16);
/// The largest thing drawn (a notification), for a fixed-size window.
#[cfg(target_os = "linux")]
pub const WINDOW_SIZE: (f64, f64) = (NOTICE_MAX_WIDTH, NOTICE_HEIGHT);
const HANDS_FREE: (f64, f64) = (102.5, 30.0);

/// Pill fill: Wispr's black, opaque while dictating, 50 % at rest.
pub const WISPR_FILL: [u8; 3] = [0x00, 0x00, 0x00];
/// While a dictation goes to the local model.
pub const LOCAL_FILL: [u8; 3] = [0x15, 0x80, 0x3d];
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

/// Notification ("condensed"): padding 8x16, Figtree 15/20 px, buttons with
/// 6x10 padding and radius 8, a 3 px progress bar along the bottom.
const NOTICE_HEIGHT: f64 = 48.0;
const NOTICE_MAX_WIDTH: f64 = 440.0;
const NOTICE_PAD: f64 = 16.0;
const NOTICE_RADIUS: f64 = 24.0;
const NOTICE_GAP: f64 = 16.0;
const NOTICE_BUTTON_PAD: f64 = 10.0;
const NOTICE_BUTTON_GAP: f64 = 8.0;
const TEXT_SIZE: f64 = 15.0;
const TEXT_LINE: f64 = 20.0;
const TEXT: [u8; 4] = [0xfc, 0xfc, 0xfb, 0xff];
const BUTTON_FILL: [u8; 4] = [0xff, 0xff, 0xff, 0x14];
const BUTTON_HOVER: [u8; 4] = [0xff, 0xff, 0xff, 0x1f];
const TRACK: [u8; 4] = [0x9d, 0x9c, 0x98, 0x66];
const PROGRESS: [u8; 4] = [0xff, 0xff, 0xeb, 0xff];
const ACTION_SHOWN: Duration = Duration::from_millis(600);

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
    Notice,
}

impl Mode {
    /// Pill size, CSS pixels (Wispr's horizontal layout); a notification's
    /// depends on its text.
    fn size(self) -> (f64, f64) {
        match self {
            Mode::Hidden | Mode::Resting => (40.0, 8.0),
            Mode::Initializing | Mode::PushToTalk => (73.0, 30.0),
            Mode::HandsFree => HANDS_FREE,
            Mode::Processing => (98.0, 30.0),
            Mode::Error => (91.0, 30.0),
            Mode::Notice => (NOTICE_MAX_WIDTH, NOTICE_HEIGHT),
        }
    }
}

/// Something the bar reports to the shim.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BarEvent {
    LocalAlternative(u64),
    /// "start", "stop" or "cancel".
    Click(&'static str),
    /// A notification button, by index.
    NoticeAction(usize),
    /// The notification closed: "timeout" or "action".
    NoticeClosed(&'static str),
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

/// Centre x of the hands-free cancel and accept buttons.
fn buttons() -> (f64, f64) {
    let content = BUTTON + GAP + WAVE_BOX + GAP + BUTTON;
    let left = (HANDS_FREE.0 - content) / 2.0;
    (left + BUTTON / 2.0, left + content - BUTTON / 2.0)
}

/// What a click at `x` (CSS pixels in the pill) means in a dictation mode.
fn click_action(mode: Mode, x: f64) -> Option<&'static str> {
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

/// Turns a dictation's volumes (dB) into 0..1 levels the way Wispr does
/// for its own: against the quietest volume heard so far (but no lower than
/// -60 dB), with the 20 dB above it filling the bar. A fixed -60..0 dB scale
/// leaves speech at about half height.
pub struct LevelScale {
    floor: f64,
}

impl LevelScale {
    pub fn new() -> LevelScale {
        LevelScale { floor: 0.0 }
    }

    pub fn level(&mut self, db: f64) -> f64 {
        if !db.is_finite() {
            return 0.0;
        }
        if db < self.floor {
            self.floor = db.max(-60.0);
        }
        ((db - self.floor) / 20.0).clamp(0.0, 1.0)
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
    Processing(Vec<Processing>),
    Destination(Option<std::sync::Arc<crate::icon::Icon>>),
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
    /// Show a notification: title, button labels, timeout, and whether it
    /// is a local dictation's (drawn in its fill).
    Notify {
        title: String,
        actions: Vec<String>,
        timeout: Duration,
        local: bool,
    },
    /// Main closed the notification.
    NotifyClear,
    /// Pill fills: for Wispr (also at rest and for notifications) and for
    /// dictations to the local model.
    Fills {
        wispr: [u8; 3],
        local: [u8; 3],
    },
    /// This dictation goes to the local model: its fill until the bar rests.
    Local,
}

#[derive(Clone)]
pub struct Processing {
    pub id: u64,
    pub local: bool,
    pub retry: bool,
    pub queued: bool,
    pub icon: Option<std::sync::Arc<crate::icon::Icon>>,
}

pub struct PlacedIcon {
    pub icon: std::sync::Arc<crate::icon::Icon>,
    pub x: f64,
    pub y: f64,
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

    fn contains(&self, x: f64, y: f64) -> bool {
        (x - self.cx).abs() <= self.w / 2.0 && (y - self.cy).abs() <= self.h / 2.0
    }
}

/// One line of text: its line box starts at (`x`, `top`). `bg` is the colour
/// under it, for drawing code that cannot blend.
#[derive(Debug, Clone, PartialEq)]
pub struct Text {
    pub x: f64,
    pub top: f64,
    pub line: f64,
    pub size: f64,
    /// Semibold (Wispr's 550) rather than regular.
    pub bold: bool,
    pub color: [u8; 4],
    pub bg: [u8; 4],
    pub text: String,
}

/// Text width in CSS pixels, from the platform's font: (text, size, bold).
pub type Measure<'a> = &'a dyn Fn(&str, f64, bool) -> f64;

/// One frame to draw.
pub struct Frame {
    pub mode: Mode,
    /// Pill size; shapes and texts are relative to its top left.
    pub size: (f64, f64),
    /// The pill first, then everything on it.
    pub shapes: Vec<Shape>,
    pub texts: Vec<Text>,
    pub icons: Vec<PlacedIcon>,
    /// Whether the next frame differs (animation running).
    pub animated: bool,
    /// For the shim.
    pub events: Vec<BarEvent>,
}

struct Notice {
    title: String,
    actions: Vec<String>,
    timeout: Duration,
    /// 0..1 of the timeout, not counting time with the pointer on it.
    progress: f64,
    /// The clicked button and when.
    clicked: Option<(usize, Instant)>,
    /// Button areas in the last frame.
    buttons: Vec<Shape>,
    /// A local dictation's: the local fill.
    local: bool,
}

/// The bar's state, fed with commands; the drawing code asks it what to show.
pub struct BarState {
    indicator: Indicator,
    notice: Option<Notice>,
    resting_bar: bool,
    enabled: bool,
    mic_active: bool,
    waving: bool,
    command_mode: bool,
    target: f64,
    level: f64,
    last_frame: Instant,
    start: Instant,
    /// The mode of the last frame, for clicks.
    shown: Mode,
    wispr_fill: [u8; 3],
    local_fill: [u8; 3],
    /// The dictation shown goes to the local model.
    local: bool,
    destination: Option<std::sync::Arc<crate::icon::Icon>>,
    icon_shown: bool,
    processing: Vec<Processing>,
    main_origin: (f64, f64),
    retry_buttons: Vec<(u64, Shape)>,
}

impl Default for BarState {
    fn default() -> BarState {
        let now = Instant::now();
        BarState {
            indicator: Indicator::Rest,
            notice: None,
            resting_bar: false,
            enabled: true,
            mic_active: false,
            waving: false,
            command_mode: false,
            target: 0.0,
            level: 0.0,
            last_frame: now,
            start: now,
            shown: Mode::Hidden,
            wispr_fill: WISPR_FILL,
            local_fill: LOCAL_FILL,
            local: false,
            destination: None,
            icon_shown: false,
            processing: Vec::new(),
            main_origin: (0., 0.),
            retry_buttons: Vec::new(),
        }
    }
}

impl BarState {
    fn recording(&self) -> bool {
        matches!(self.indicator, Indicator::PushToTalk | Indicator::HandsFree)
    }

    /// The pill's fill, with `alpha`.
    fn fill(&self, alpha: u8) -> [u8; 4] {
        let [r, g, b] = if self.local {
            self.local_fill
        } else {
            self.wispr_fill
        };
        [r, g, b, alpha]
    }

    fn rest(&mut self) {
        self.indicator = Indicator::Rest;
        self.destination = None;
        self.local = false;
        self.command_mode = false;
        self.mic_active = false;
        self.waving = false;
    }

    pub fn apply(&mut self, command: PillCommand) {
        match command {
            PillCommand::Processing(jobs) => self.processing = jobs,
            PillCommand::Destination(icon) => self.destination = icon,
            PillCommand::Status(s) => match s.as_str() {
                "initializing" => {
                    // A new dictation closes a notification (main clears it too).
                    self.notice = None;
                    self.indicator = Indicator::Initializing;
                    self.waving = false;
                    // Wispr's own: a local dictation ended before it.
                    self.local = false;
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
            PillCommand::Notify {
                title,
                actions,
                timeout,
                local,
            } => {
                self.notice = Some(Notice {
                    title,
                    actions: actions.into_iter().take(2).collect(),
                    timeout,
                    progress: 0.0,
                    clicked: None,
                    buttons: Vec::new(),
                    local,
                })
            }
            PillCommand::NotifyClear => self.notice = None,
            PillCommand::Fills { wispr, local } => {
                self.wispr_fill = wispr;
                self.local_fill = local;
            }
            PillCommand::Local => self.local = true,
        }
        if !self.recording() {
            self.target = 0.0;
        }
    }

    /// The mode on screen: Hidden while Wispr's own page draws the bar.
    fn mode(&self, now: Instant) -> Mode {
        if !self.enabled {
            return Mode::Hidden;
        }
        if self.notice.is_some() {
            return Mode::Notice;
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

    /// A left click at `(x, y)` CSS pixels in the pill drawn last.
    pub fn click(&mut self, x: f64, y: f64) -> Option<BarEvent> {
        if let Some((id, _)) = self
            .retry_buttons
            .iter()
            .find(|(_, shape)| shape.contains(x, y))
        {
            return Some(BarEvent::LocalAlternative(*id));
        }
        let (x, y) = (x - self.main_origin.0, y - self.main_origin.1);
        if y < 0. {
            return None;
        }
        if self.shown == Mode::Notice {
            let n = self.notice.as_mut()?;
            if n.clicked.is_some() {
                return None;
            }
            let i = n.buttons.iter().position(|b| b.contains(x, y))?;
            n.clicked = Some((i, Instant::now()));
            return Some(BarEvent::NoticeAction(i));
        }
        click_action(self.shown, x - if self.icon_shown { 26.0 } else { 0.0 }).map(BarEvent::Click)
    }

    /// Advances the animations to `now` and returns what to draw. `pointer`
    /// is the mouse position in the pill, if it is on it.
    pub fn frame(&mut self, now: Instant, measure: Measure, pointer: Option<(f64, f64)>) -> Frame {
        if let Indicator::Error(until) = self.indicator {
            if now >= until {
                self.rest();
            }
        }
        let dt = now.duration_since(self.last_frame);
        self.level = wave::smooth(self.level, self.target, dt.as_secs_f64() * 1000.0);
        self.last_frame = now;
        let mut events = Vec::new();
        if let Some(closed) = self.advance_notice(now, dt, pointer.is_some()) {
            events.push(closed);
        }
        let t = now.duration_since(self.start).as_secs_f64();
        let mode = self.mode(now);
        self.shown = mode;
        let mut shapes = Vec::new();
        let mut texts = Vec::new();
        let (size, animated) = match mode {
            Mode::Hidden => (mode.size(), false),
            Mode::Resting => {
                let size = mode.size();
                shapes.push(pill(size, self.fill(0x80), REST_OUTLINE, 1.0));
                (size, false)
            }
            Mode::Notice => match self.notice.as_mut() {
                Some(n) => {
                    let [r, g, b] = if n.local {
                        self.local_fill
                    } else {
                        self.wispr_fill
                    };
                    let fill = [r, g, b, 0xff];
                    (
                        n.draw(measure, pointer, fill, &mut shapes, &mut texts),
                        true,
                    )
                }
                None => (mode.size(), false),
            },
            _ => {
                let size = mode.size();
                let outline = match mode {
                    Mode::Error => (ERROR_OUTLINE, 1.5),
                    _ if self.command_mode => ([COMMAND[0], COMMAND[1], COMMAND[2], 0xff], 1.0),
                    _ => (OUTLINE, 1.0),
                };
                shapes.push(pill(size, self.fill(0xff), outline.0, outline.1));
                (size, self.content(mode, size, t, &mut shapes))
            }
        };
        let icon = self.destination.clone().filter(|_| {
            matches!(
                mode,
                Mode::Initializing | Mode::PushToTalk | Mode::HandsFree | Mode::Processing
            )
        });
        self.icon_shown = icon.is_some();
        let mut size = size;
        if self.icon_shown {
            size.0 += 26.0;
            if let Some(body) = shapes.first_mut() {
                body.w += 26.0;
                body.cx += 13.0;
            }
            for shape in shapes.iter_mut().skip(1) {
                shape.cx += 26.0;
            }
        }
        let mut icons = Vec::new();
        let base_size = size;
        let row_width = |job: &Processing| {
            98.0 + if job.icon.is_some() { 26.0 } else { 0.0 } + if job.retry { 30.0 } else { 0.0 }
        };
        let width = self.processing.iter().map(row_width).fold(size.0, f64::max);
        let extra = if self.enabled {
            self.processing.len() as f64 * 38.0
        } else {
            0.0
        };
        self.main_origin = ((width - base_size.0) / 2.0, extra);
        for shape in &mut shapes {
            shape.cx += self.main_origin.0;
            shape.cy += extra;
        }
        for text in &mut texts {
            text.x += self.main_origin.0;
            text.top += extra;
        }
        if let Some(icon) = &icon {
            icons.push(PlacedIcon {
                icon: icon.clone(),
                x: self.main_origin.0 + 8.0,
                y: extra + 6.0,
            });
        }
        self.retry_buttons.clear();
        if self.enabled {
            for (index, job) in self.processing.iter().enumerate() {
                let w = row_width(job);
                let (x, y) = ((width - w) / 2.0, index as f64 * 38.0);
                let color = if job.local {
                    self.local_fill
                } else {
                    self.wispr_fill
                };
                shapes.push(Shape::rect(
                    x,
                    y,
                    w,
                    30.,
                    15.,
                    [color[0], color[1], color[2], 0xff],
                ));
                let inset = if let Some(icon) = &job.icon {
                    icons.push(PlacedIcon {
                        icon: icon.clone(),
                        x: x + 8.,
                        y: y + 6.,
                    });
                    26.
                } else {
                    0.
                };
                let spinner_x = x + inset + 45.;
                for i in 0..8 {
                    let angle = i as f64 * std::f64::consts::PI / 4.;
                    let alpha = if job.queued {
                        0x70
                    } else {
                        (spinner_alpha(t, i) * 255.) as u8
                    };
                    shapes.push(Shape {
                        cx: spinner_x + angle.cos() * 6.,
                        cy: y + 15. + angle.sin() * 6.,
                        w: 3.,
                        h: 2.,
                        angle,
                        radius: 1.,
                        fill: [SPINNER[0], SPINNER[1], SPINNER[2], alpha],
                        border: None,
                    });
                }
                if job.retry {
                    let button =
                        Shape::rect(x + w - 27., y + 4., 22., 22., 11., [0x15, 0x80, 0x3d, 0xff]);
                    let (cx, cy) = (button.cx, button.cy);
                    shapes.push(button);
                    self.retry_buttons.push((job.id, button));
                    for i in 0..8 {
                        let a = (i as f64 * 35. + 20.).to_radians();
                        let b = ((i + 1) as f64 * 35. + 20.).to_radians();
                        shapes.push(Shape::line(
                            (cx + a.cos() * 5., cy + a.sin() * 5.),
                            (cx + b.cos() * 5., cy + b.sin() * 5.),
                            1.4,
                            [0xff; 4],
                        ));
                    }
                    shapes.push(Shape::line(
                        (cx + 5., cy - 4.),
                        (cx + 5., cy),
                        1.5,
                        [0xff; 4],
                    ));
                    shapes.push(Shape::line(
                        (cx + 5., cy),
                        (cx + 1., cy - 1.),
                        1.5,
                        [0xff; 4],
                    ));
                }
            }
        }
        size = (width, base_size.1 + extra);
        Frame {
            mode: if extra > 0. && mode == Mode::Hidden {
                Mode::Processing
            } else {
                mode
            },
            size,
            icons,
            shapes,
            texts,
            animated: animated || extra > 0.,
            events,
        }
    }

    /// Runs the notification's timer; returns its close event when it ends.
    fn advance_notice(&mut self, now: Instant, dt: Duration, hovered: bool) -> Option<BarEvent> {
        let n = self.notice.as_mut()?;
        let reason = match n.clicked {
            Some((_, at)) if now.duration_since(at) >= ACTION_SHOWN => Some("action"),
            Some(_) => None,
            None => {
                if !hovered && self.shown == Mode::Notice && !n.timeout.is_zero() {
                    n.progress += dt.as_secs_f64() / n.timeout.as_secs_f64();
                }
                (n.progress >= 1.0).then_some("timeout")
            }
        };
        if reason.is_some() {
            self.notice = None;
        }
        reason.map(BarEvent::NoticeClosed)
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

impl Notice {
    /// Lays out and draws the notification; returns its size.
    fn draw(
        &mut self,
        measure: Measure,
        pointer: Option<(f64, f64)>,
        fill: [u8; 4],
        shapes: &mut Vec<Shape>,
        texts: &mut Vec<Text>,
    ) -> (f64, f64) {
        let title_w = measure(&self.title, TEXT_SIZE, true).ceil();
        let labels: Vec<f64> = self
            .actions
            .iter()
            .map(|a| measure(a, TEXT_SIZE, false).ceil())
            .collect();
        let buttons_w: f64 = labels
            .iter()
            .map(|w| w + 2.0 * NOTICE_BUTTON_PAD)
            .sum::<f64>()
            + NOTICE_BUTTON_GAP * labels.len().saturating_sub(1) as f64;
        let gap = if labels.is_empty() { 0.0 } else { NOTICE_GAP };
        let w = (NOTICE_PAD * 2.0 + title_w + gap + buttons_w).min(NOTICE_MAX_WIDTH);
        let h = NOTICE_HEIGHT;
        shapes.push(Shape::rect(0.0, 0.0, w, h, NOTICE_RADIUS, fill));
        let text_top = (h - TEXT_LINE) / 2.0;
        texts.push(Text {
            x: NOTICE_PAD,
            top: text_top,
            line: TEXT_LINE,
            size: TEXT_SIZE,
            bold: true,
            color: TEXT,
            bg: fill,
            text: self.title.clone(),
        });

        self.buttons.clear();
        let mut x = w - NOTICE_PAD - buttons_w;
        for (i, (label, lw)) in self.actions.iter().zip(&labels).enumerate() {
            let bw = lw + 2.0 * NOTICE_BUTTON_PAD;
            let hovered = pointer
                .is_some_and(|(px, py)| px >= x && px <= x + bw && py >= 8.0 && py <= h - 8.0);
            let button = Shape::rect(
                x,
                8.0,
                bw,
                h - 16.0,
                8.0,
                if hovered { BUTTON_HOVER } else { BUTTON_FILL },
            );
            shapes.push(button);
            self.buttons.push(button);
            match self.clicked {
                // Wispr swaps the label for a check while the action runs.
                Some((c, _)) if c == i => check_icon(x + bw / 2.0, h / 2.0, 16.0, TEXT, shapes),
                _ => texts.push(Text {
                    x: x + NOTICE_BUTTON_PAD,
                    top: text_top,
                    line: TEXT_LINE,
                    size: TEXT_SIZE,
                    bold: false,
                    color: TEXT,
                    bg: over(button.fill, fill),
                    text: label.clone(),
                }),
            }
            x += bw + NOTICE_BUTTON_GAP;
        }

        if !self.timeout.is_zero() {
            // Along the bottom edge, inside the rounded corners.
            let (left, right) = (NOTICE_RADIUS / 2.0, w - NOTICE_RADIUS / 2.0);
            shapes.push(Shape::rect(left, h - 3.0, right - left, 3.0, 1.5, TRACK));
            let done = (right - left) * self.progress.clamp(0.0, 1.0);
            if done > 0.0 {
                shapes.push(Shape::rect(left, h - 3.0, done, 3.0, 1.5, PROGRESS));
            }
        }
        (w, h)
    }
}

fn pill(size: (f64, f64), fill: [u8; 4], outline: [u8; 4], width: f64) -> Shape {
    let mut s = Shape::rect(0.0, 0.0, size.0, size.1, size.1 / 2.0, fill);
    s.border = Some((outline, width));
    s
}

/// A lucide icon line (24-unit viewBox) drawn `size` pixels wide at stroke
/// `stroke` units.
fn icon_line(
    cx: f64,
    cy: f64,
    size: f64,
    stroke: f64,
    a: (f64, f64),
    b: (f64, f64),
    fill: [u8; 4],
) -> Shape {
    let k = size / 24.0;
    let p = |(x, y): (f64, f64)| (cx + (x - 12.0) * k, cy + (y - 12.0) * k);
    Shape::line(p(a), p(b), stroke * k, fill)
}

fn cancel_button(cx: f64, cy: f64, shapes: &mut Vec<Shape>) {
    let r = BUTTON / 2.0;
    shapes.push(Shape::rect(cx - r, cy - r, BUTTON, BUTTON, r, CANCEL_BG));
    // lucide x, 12 px, stroke 3: M18 6 6 18, m6 6 12 12
    shapes.push(icon_line(
        cx,
        cy,
        12.0,
        3.0,
        (18.0, 6.0),
        (6.0, 18.0),
        CANCEL_FG,
    ));
    shapes.push(icon_line(
        cx,
        cy,
        12.0,
        3.0,
        (6.0, 6.0),
        (18.0, 18.0),
        CANCEL_FG,
    ));
}

fn accept_button(cx: f64, cy: f64, shapes: &mut Vec<Shape>) {
    let r = BUTTON / 2.0;
    shapes.push(Shape::rect(cx - r, cy - r, BUTTON, BUTTON, r, ACCEPT_BG));
    shapes.push(icon_line(
        cx,
        cy,
        12.0,
        3.0,
        (20.0, 6.0),
        (9.0, 17.0),
        ACCEPT_FG,
    ));
    shapes.push(icon_line(
        cx,
        cy,
        12.0,
        3.0,
        (9.0, 17.0),
        (4.0, 12.0),
        ACCEPT_FG,
    ));
}

/// lucide check (M20 6 9 17l-5-5), stroke 2.
fn check_icon(cx: f64, cy: f64, size: f64, fill: [u8; 4], shapes: &mut Vec<Shape>) {
    shapes.push(icon_line(cx, cy, size, 2.0, (20.0, 6.0), (9.0, 17.0), fill));
    shapes.push(icon_line(cx, cy, size, 2.0, (9.0, 17.0), (4.0, 12.0), fill));
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
    #[test]
    fn processing_stacks_above_recording_and_green_buttons_keep_their_job() {
        use super::*;
        let mut state = BarState::default();
        state.apply(PillCommand::Status("listening".into()));
        state.apply(PillCommand::Processing(vec![
            Processing {
                id: 11,
                local: false,
                retry: true,
                queued: false,
                icon: None,
            },
            Processing {
                id: 12,
                local: false,
                retry: false,
                queued: true,
                icon: None,
            },
            Processing {
                id: 13,
                local: true,
                retry: false,
                queued: false,
                icon: None,
            },
        ]));
        let frame = state.frame(Instant::now(), &|_, _, _| 0., None);
        assert_eq!(frame.size.1, 30. + 3. * 38.);
        assert_eq!(state.retry_buttons.len(), 1);
        let button = state.retry_buttons[0].1;
        assert_eq!(button.fill, [0x15, 0x80, 0x3d, 0xff]);
        assert_eq!(
            state.click(button.cx, button.cy),
            Some(BarEvent::LocalAlternative(11))
        );
        assert_eq!(
            state.click(frame.size.0 / 2., 3. * 38. + 15.),
            Some(BarEvent::Click("stop"))
        );
        assert_eq!(state.click(frame.size.0 / 2., 76. + 15.), None);
        state.apply(PillCommand::Processing(vec![]));
        assert_eq!(state.frame(Instant::now(), &|_, _, _| 0., None).size.1, 30.);
    }
    #[test]
    fn destination_icon_expands_content_and_preserves_hands_free_buttons() {
        let mut state = super::BarState::default();
        state.apply(super::PillCommand::Popo);
        let now = std::time::Instant::now();
        let measure = |_: &str, _: f64, _: bool| 0.0;
        let plain = state.frame(now, &measure, None);
        let icon = crate::icon::Icon::load("data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' width='8' height='8'><rect width='8' height='8'/></svg>").unwrap();
        state.apply(super::PillCommand::Destination(Some(std::sync::Arc::new(
            icon,
        ))));
        let routed = state.frame(now, &measure, None);
        assert_eq!(routed.size.0, plain.size.0 + 26.0);
        assert_eq!(routed.icons.len(), 1);
        let (cancel, accept) = super::buttons();
        assert_eq!(
            state.click(cancel + 26.0, 15.0),
            Some(super::BarEvent::Click("cancel"))
        );
        assert_eq!(
            state.click(accept + 26.0, 15.0),
            Some(super::BarEvent::Click("stop"))
        );
        state.apply(super::PillCommand::Status("processing".into()));
        assert_eq!(state.frame(now, &measure, None).icons.len(), 1);
        state.apply(super::PillCommand::Status("idle".into()));
        assert!(state.frame(now, &measure, None).icons.is_empty());
    }

    use super::*;

    /// 8 px per character.
    fn measure(text: &str, _size: f64, _bold: bool) -> f64 {
        8.0 * text.chars().count() as f64
    }

    fn state() -> BarState {
        let mut s = BarState::default();
        s.apply(PillCommand::RestingBar(true));
        s
    }

    fn now() -> Instant {
        Instant::now()
    }

    fn mode(s: &mut BarState) -> Mode {
        s.frame(now(), &measure, None).mode
    }

    fn fill(s: &mut BarState) -> [u8; 4] {
        s.frame(now(), &measure, None).shapes[0].fill
    }

    #[test]
    fn the_pill_has_the_colour_of_where_the_dictation_goes() {
        let mut s = state();
        assert_eq!(fill(&mut s), [0, 0, 0, 0x80], "Wispr's black at rest");
        s.apply(PillCommand::Fills {
            wispr: [0xc8, 0x14, 0x14],
            local: [1, 2, 3],
        });
        assert_eq!(fill(&mut s), [0xc8, 0x14, 0x14, 0x80]);
        s.apply(PillCommand::Local);
        s.apply(PillCommand::Status("listening".into()));
        assert_eq!(fill(&mut s), [1, 2, 3, 0xff]);
        s.apply(PillCommand::Status("processing".into()));
        assert_eq!(fill(&mut s), [1, 2, 3, 0xff], "until the text is out");
        s.apply(PillCommand::Status("idle".into()));
        assert_eq!(fill(&mut s), [0xc8, 0x14, 0x14, 0x80]);
        // Wispr's own dictation after a local one.
        s.apply(PillCommand::Local);
        s.apply(PillCommand::Status("initializing".into()));
        assert_eq!(fill(&mut s), [0xc8, 0x14, 0x14, 0xff]);
    }

    #[test]
    fn push_to_talk_goes_through_wispr_states() {
        let mut s = state();
        assert_eq!(mode(&mut s), Mode::Resting);
        s.apply(PillCommand::Status("initializing".into()));
        assert_eq!(mode(&mut s), Mode::Initializing);
        s.apply(PillCommand::Status("listening".into()));
        assert_eq!(mode(&mut s), Mode::PushToTalk);
        s.apply(PillCommand::Status("stopping".into()));
        assert_eq!(mode(&mut s), Mode::Processing);
        s.apply(PillCommand::Status("idle".into()));
        assert_eq!(mode(&mut s), Mode::Resting);
    }

    #[test]
    fn bars_move_only_once_the_mic_is_active_while_recording() {
        let mut s = state();
        s.apply(PillCommand::Status("initializing".into()));
        s.apply(PillCommand::Mic(true));
        assert!(
            !s.frame(now(), &measure, None).animated,
            "dropped while initializing, as in Wispr"
        );
        s.apply(PillCommand::Status("listening".into()));
        assert!(
            !s.frame(now(), &measure, None).animated,
            "still dots until the first audio"
        );
        s.apply(PillCommand::Mic(true));
        s.apply(PillCommand::Level(1.0));
        let f = s.frame(now() + Duration::from_secs(1), &measure, None);
        assert!(f.animated);
        let tallest = f.shapes[1..].iter().map(|b| b.h).fold(0.0, f64::max);
        assert!(
            tallest > 2.0 * wave::BAR_BASE,
            "a loud voice makes tall bars"
        );
        s.apply(PillCommand::Silence(true));
        assert!(!s.frame(now(), &measure, None).animated);
    }

    #[test]
    fn hands_free_keeps_its_pill_and_its_buttons_work() {
        let mut s = state();
        s.apply(PillCommand::Status("initializing".into()));
        s.apply(PillCommand::Popo);
        s.apply(PillCommand::Status("listening".into()));
        let f = s.frame(now(), &measure, None);
        assert_eq!((f.mode, f.size), (Mode::HandsFree, (102.5, 30.0)));
        let (cancel, accept) = buttons();
        assert_eq!(s.click(cancel, 15.0), Some(BarEvent::Click("cancel")));
        assert_eq!(s.click(accept, 15.0), Some(BarEvent::Click("stop")));
        assert_eq!(s.click(HANDS_FREE.0 / 2.0, 15.0), None);
        // [cancel 18] 4 [waveform 46] 4 [accept 18], centred.
        assert!((accept - cancel - (18.0 + 4.0 + 46.0 + 4.0)).abs() < 1e-9);
        assert!((cancel - 9.0 - (HANDS_FREE.0 - 90.0) / 2.0).abs() < 1e-9);
    }

    #[test]
    fn rest_and_push_to_talk_clicks() {
        let mut s = state();
        mode(&mut s);
        assert_eq!(s.click(10.0, 4.0), Some(BarEvent::Click("start")));
        s.apply(PillCommand::Status("listening".into()));
        mode(&mut s);
        assert_eq!(s.click(10.0, 15.0), Some(BarEvent::Click("stop")));
        s.apply(PillCommand::Status("processing".into()));
        mode(&mut s);
        assert_eq!(s.click(10.0, 15.0), None);
    }

    #[test]
    fn errors_flash_then_rest() {
        let mut s = state();
        s.apply(PillCommand::Status("error".into()));
        assert_eq!(mode(&mut s), Mode::Error);
        let later = now() + Duration::from_millis(600);
        assert_eq!(s.frame(later, &measure, None).mode, Mode::Resting);
    }

    #[test]
    fn wispr_page_hides_the_bar() {
        let mut s = state();
        s.apply(PillCommand::Enabled(false));
        let f = s.frame(now(), &measure, None);
        assert_eq!(f.mode, Mode::Hidden);
        assert!(f.shapes.is_empty());
    }

    fn cancelled(s: &mut BarState) {
        s.apply(PillCommand::Notify {
            title: "Transcript cancelled".into(),
            actions: vec!["Undo".into()],
            timeout: Duration::from_secs(3),
            local: false,
        });
    }

    #[test]
    fn a_notification_replaces_the_bar_and_times_out() {
        let mut s = state();
        cancelled(&mut s);
        let t0 = now();
        let f = s.frame(t0, &measure, None);
        assert_eq!(f.mode, Mode::Notice);
        // 16 + 160 (title) + 16 + 10 + 32 (Undo) + 10 + 16
        assert_eq!(f.size, (260.0, 48.0));
        assert_eq!(f.texts[0].text, "Transcript cancelled");
        assert_eq!(f.texts[1].text, "Undo");
        let f = s.frame(t0 + Duration::from_millis(3100), &measure, None);
        assert_eq!(f.events, vec![BarEvent::NoticeClosed("timeout")]);
        assert_eq!(f.mode, Mode::Resting);
    }

    #[test]
    fn hovering_pauses_the_notification() {
        let mut s = state();
        cancelled(&mut s);
        let t0 = now();
        s.frame(t0, &measure, None);
        let f = s.frame(t0 + Duration::from_secs(5), &measure, Some((20.0, 20.0)));
        assert!(f.events.is_empty() && f.mode == Mode::Notice);
        let f = s.frame(t0 + Duration::from_secs(9), &measure, None);
        assert_eq!(f.events, vec![BarEvent::NoticeClosed("timeout")]);
    }

    #[test]
    fn undo_reports_the_action_shows_a_check_then_closes() {
        let mut s = state();
        cancelled(&mut s);
        s.frame(now(), &measure, None);
        // Undo sits at the right: x from 260 - 16 - 52 = 192 to 244.
        assert_eq!(s.click(100.0, 24.0), None, "the title is not a button");
        assert_eq!(s.click(210.0, 24.0), Some(BarEvent::NoticeAction(0)));
        assert_eq!(s.click(210.0, 24.0), None, "only once");
        let f = s.frame(now(), &measure, None);
        assert_eq!(f.texts.len(), 1, "the label became a check");
        let f = s.frame(now() + Duration::from_millis(700), &measure, None);
        assert_eq!(f.events, vec![BarEvent::NoticeClosed("action")]);
    }

    #[test]
    fn a_new_dictation_or_main_closes_the_notification() {
        let mut s = state();
        cancelled(&mut s);
        s.apply(PillCommand::Status("initializing".into()));
        assert_eq!(mode(&mut s), Mode::Initializing);
        cancelled(&mut s);
        s.apply(PillCommand::NotifyClear);
        assert_eq!(mode(&mut s), Mode::Initializing);
    }

    #[test]
    fn level_scale_follows_the_quietest_volume() {
        let mut s = LevelScale::new();
        // The first volume sets the floor: nothing to show yet.
        assert_eq!(s.level(-50.0), 0.0);
        assert!((s.level(-40.0) - 0.5).abs() < 1e-9);
        assert_eq!(s.level(-25.0), 1.0);
        // Silence lowers the floor, but not below -60 dB.
        assert_eq!(s.level(-200.0), 0.0);
        assert!((s.level(-50.0) - 0.5).abs() < 1e-9);
        assert_eq!(s.level(f64::NAN), 0.0);
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
