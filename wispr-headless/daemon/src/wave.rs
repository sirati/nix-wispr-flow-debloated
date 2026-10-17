//! Wispr's waveform, from its status page (renderer/status, the Waveform
//! component and its CSS), so the bars move as they do in Wispr:
//!
//! - ten bars, 2 CSS px wide with a 2 px base height, white while the mic is
//!   active and white at 40 % otherwise;
//! - each bar is scaled by `audio scale x height scale x keyframe`, where
//!   the audio scale is `max(1, 5 x level)` with the level smoothed every frame
//!   as `0.85 x previous + 0.15 x latest` (at 60 fps), the height scale is
//!   `max(0, 1 - d^2 / 48)` for the distance `d` from the middle, and the
//!   keyframe is a 1 s loop 1, 1.2, 1.5, 1.1, 1.3, 1 (at 0, .2, .4, .8, .9, 1),
//!   each bar starting `0.1 s x its offset from the middle` later.
//!
//! Sizes here are in CSS pixels; the bar scales them to the screen.

pub const BARS: usize = 10;
/// Width and base height of one bar, in CSS pixels.
pub const BAR_WIDTH: f64 = 2.0;
pub const BAR_BASE: f64 = 2.0;
const GAIN: f64 = 5.0;
const KEYFRAMES: [(f64, f64); 6] = [
    (0.0, 1.0),
    (0.2, 1.2),
    (0.4, 1.5),
    (0.8, 1.1),
    (0.9, 1.3),
    (1.0, 1.0),
];
const FRAME_MS: f64 = 1000.0 / 60.0;

/// One smoothing step over `dt_ms`, as Wispr does it per animation frame,
/// rounded down to hundredths like Wispr.
pub fn smooth(previous: f64, target: f64, dt_ms: f64) -> f64 {
    let keep = 0.85f64.powf(dt_ms.max(0.0) / FRAME_MS);
    let v = previous * keep + target * (1.0 - keep);
    (v * 100.0).floor() / 100.0
}

pub fn audio_scale(level: f64) -> f64 {
    (GAIN * level).max(1.0)
}

/// `max(0, 1 - d^2 / 48)` for a bar's distance from the middle.
pub fn height_scale(bar: usize) -> f64 {
    let d = ((BARS - 1) as f64 / 2.0 - bar as f64).abs();
    (1.0 - d * d / 48.0).max(0.0)
}

/// Offset from the middle that sets each bar's animation delay: 0..4 for the
/// left half, -5..-1 for the right half, as in Wispr.
pub fn delay_index(bar: usize) -> f64 {
    let half = BARS.div_ceil(2);
    if bar < half {
        bar as f64
    } else {
        bar as f64 - BARS as f64
    }
}

fn smoothstep(x: f64) -> f64 {
    x * x * (3.0 - 2.0 * x)
}

/// The keyframe multiplier at time `t` seconds into the loop.
pub fn keyframe(t: f64) -> f64 {
    let p = t - t.floor();
    for w in KEYFRAMES.windows(2) {
        let ((t0, v0), (t1, v1)) = (w[0], w[1]);
        if p <= t1 {
            let span = t1 - t0;
            let f = if span > 0.0 { (p - t0) / span } else { 1.0 };
            return v0 + (v1 - v0) * smoothstep(f);
        }
    }
    KEYFRAMES[KEYFRAMES.len() - 1].1
}

/// Heights of all bars in CSS pixels at time `t` seconds, for a smoothed
/// level.
pub fn heights(t: f64, level: f64) -> [f64; BARS] {
    let scale = audio_scale(level);
    let mut out = [0.0; BARS];
    for (i, h) in out.iter_mut().enumerate() {
        *h = BAR_BASE * scale * height_scale(i) * keyframe(t - 0.1 * delay_index(i));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoothing_matches_wispr_at_60_fps() {
        // One frame: 0.85 * 0 + 0.15 * 1 = 0.15.
        assert_eq!(smooth(0.0, 1.0, FRAME_MS), 0.15);
        // Two frames at once equal two single frames (before rounding).
        let two = smooth(0.0, 1.0, 2.0 * FRAME_MS);
        assert!((two - (0.15 * 0.85 + 0.15)).abs() < 0.011, "{two}");
        assert_eq!(smooth(0.5, 0.5, 33.0), 0.5);
    }

    #[test]
    fn quiet_input_keeps_bars_at_their_floor() {
        assert_eq!(audio_scale(0.0), 1.0);
        assert_eq!(audio_scale(0.1), 1.0);
        assert_eq!(audio_scale(1.0), 5.0);
    }

    #[test]
    fn middle_bars_are_tallest() {
        let s: Vec<f64> = (0..BARS).map(height_scale).collect();
        assert!((s[4] - (1.0 - 0.25 / 48.0)).abs() < 1e-12);
        assert_eq!(s[4], s[5]);
        assert!(s[0] < s[2] && s[2] < s[4]);
        assert!((s[0] - (1.0 - 20.25 / 48.0)).abs() < 1e-12);
    }

    #[test]
    fn delays_match_wispr() {
        let d: Vec<f64> = (0..BARS).map(delay_index).collect();
        assert_eq!(
            d,
            vec![0.0, 1.0, 2.0, 3.0, 4.0, -5.0, -4.0, -3.0, -2.0, -1.0]
        );
    }

    #[test]
    fn keyframes_hit_their_values() {
        assert_eq!(keyframe(0.0), 1.0);
        assert!((keyframe(0.2) - 1.2).abs() < 1e-12);
        assert!((keyframe(0.4) - 1.5).abs() < 1e-12);
        assert!((keyframe(0.9) - 1.3).abs() < 1e-12);
        assert!((keyframe(1.4) - 1.5).abs() < 1e-12, "loops every second");
        assert!(
            (keyframe(-0.6) - 1.5).abs() < 1e-12,
            "negative times loop too"
        );
    }

    #[test]
    fn a_loud_voice_makes_tall_bars() {
        let quiet = heights(0.4, 0.0);
        let loud = heights(0.4, 1.0);
        for i in 0..BARS {
            assert!(loud[i] > quiet[i]);
        }
        // At most 2 px x 5 x 1 x 1.5.
        assert!(loud.iter().all(|&h| h <= 15.0 + 1e-9));
    }
}
