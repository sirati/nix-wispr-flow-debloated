//! What Wispr's flow bar shows, independent of how it is drawn: the small
//! resting bar at the bottom of the screen, Wispr's waveform while recording
//! (see wave.rs), and pulsing dots while Wispr finishes. A click on the resting
//! bar starts a dictation and a click while recording stops it, as on Wispr's
//! own bar. pill_x11.rs (Linux) and pill_mac.rs (macOS) draw it.
//!
//! Sizes are Wispr's CSS pixels. Measured from Wispr's own page at 200 %: the
//! resting bar is 80x16 device pixels, a black fill at 50 % inside a 2 px
//! #aaaaaa outline at 75 %.

use std::time::{Duration, Instant};

use crate::wave;

/// All sizes below are CSS pixels.
pub const WINDOW: (f64, f64) = (66.0, 18.0);
pub const REST: (f64, f64) = (40.0, 8.0);
pub const ACTIVE: (f64, f64) = (54.0, 18.0);
pub const BORDER: f64 = 1.0;
pub const BAR_GAP: f64 = 2.0;
/// Gap between the bottom of the bar and the bottom of the screen.
pub const BOTTOM_MARGIN: f64 = 10.0;
pub const FRAME: Duration = Duration::from_millis(16);
/// Working dots: diameter and distance between centres.
pub const DOT: f64 = 3.0;
pub const DOT_STEP: f64 = 6.0;

/// What the bar shows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    Hidden,
    Resting,
    Recording,
    Working,
}

impl Mode {
    /// Size of the drawn bar in this mode, CSS pixels.
    pub fn size(self) -> (f64, f64) {
        match self {
            Mode::Resting => REST,
            _ => ACTIVE,
        }
    }

    pub fn animated(self) -> bool {
        matches!(self, Mode::Recording | Mode::Working)
    }
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

/// Opacity of working dot `i` (0..3) at time `t` seconds.
pub fn dot_alpha(t: f64, i: usize) -> f64 {
    ((t * 6.0 - i as f64 * 0.9).sin() * 0.5 + 0.5) * 0.8 + 0.2
}

/// Commands for the bar.
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

/// One frame to draw.
pub struct Frame {
    pub mode: Mode,
    pub heights: [f64; wave::BARS],
    pub mic_active: bool,
    /// Seconds since the bar started, for the animations.
    pub t: f64,
}

/// The bar's state, fed with commands; the drawing code asks it what to show.
pub struct BarState {
    status: String,
    resting_bar: bool,
    enabled: bool,
    mic_active: bool,
    target: f64,
    level: f64,
    last_frame: Instant,
    start: Instant,
}

impl Default for BarState {
    fn default() -> BarState {
        let now = Instant::now();
        BarState {
            status: String::from("idle"),
            resting_bar: false,
            enabled: true,
            mic_active: false,
            target: 0.0,
            level: 0.0,
            last_frame: now,
            start: now,
        }
    }
}

impl BarState {
    pub fn apply(&mut self, command: PillCommand) {
        match command {
            PillCommand::Status(s) => {
                self.status = s;
                if self.status != "recording" {
                    self.target = 0.0;
                }
            }
            PillCommand::Level(v) => self.target = normalize_level(v),
            PillCommand::Mic(m) => self.mic_active = m,
            PillCommand::Enabled(e) => self.enabled = e,
            PillCommand::RestingBar(r) => self.resting_bar = r,
        }
    }

    /// The mode on screen: Hidden while Wispr's own page draws the bar.
    pub fn shown(&self) -> Mode {
        if self.enabled {
            mode_for(&self.status, self.resting_bar)
        } else {
            Mode::Hidden
        }
    }

    /// Advances the level smoothing to `now` and returns what to draw.
    pub fn frame(&mut self, now: Instant) -> Frame {
        let dt = now.duration_since(self.last_frame).as_secs_f64() * 1000.0;
        self.level = wave::smooth(self.level, self.target, dt);
        self.last_frame = now;
        let t = now.duration_since(self.start).as_secs_f64();
        Frame {
            mode: self.shown(),
            heights: wave::heights(t, self.level),
            mic_active: self.mic_active,
            t,
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
    fn levels_are_clamped_and_db_tolerated() {
        assert_eq!(normalize_level(0.5), 0.5);
        assert_eq!(normalize_level(3.0), 1.0);
        assert_eq!(normalize_level(f64::NAN), 0.0);
        assert!((normalize_level(-30.0) - 0.5).abs() < 1e-9);
        assert_eq!(normalize_level(-200.0), 0.0);
    }

    #[test]
    fn wispr_page_hides_the_bar_and_a_stop_resets_the_level() {
        let mut s = BarState::default();
        s.apply(PillCommand::RestingBar(true));
        assert_eq!(s.shown(), Mode::Resting);
        s.apply(PillCommand::Enabled(false));
        assert_eq!(s.shown(), Mode::Hidden);
        s.apply(PillCommand::Enabled(true));
        s.apply(PillCommand::Status("recording".into()));
        s.apply(PillCommand::Level(1.0));
        let later = Instant::now() + Duration::from_secs(1);
        assert!(s.frame(later).heights[4] > 2.0 * wave::BAR_BASE);
        s.apply(PillCommand::Status("flushing".into()));
        assert_eq!(s.shown(), Mode::Working);
        let f = s.frame(later + Duration::from_secs(2));
        assert!(f.heights[4] <= 1.5 * wave::BAR_BASE + 1e-9, "level decays");
    }

    #[test]
    fn dots_pulse_within_range() {
        for i in 0..3 {
            for k in 0..100 {
                let a = dot_alpha(k as f64 * 0.013, i);
                assert!((0.2..=1.0).contains(&a));
            }
        }
    }
}
