//! Cuts a dictation into pieces Granite Speech can take while it is still
//! being spoken. Granite is not a streaming model, so each piece is sent once
//! it is complete; short pieces leave little to do when the dictation ends.
//!
//! The audio is measured in 30 ms frames. A frame is silent when it is
//! quieter than a threshold between the dictation's noise floor (its 10th
//! percentile) and its speech (90th percentile), and always below -60 dBFS.
//! A piece ends at the first pause after its target length (5 s unless
//! `targetSeconds` says otherwise), in the middle of the pause; the closer it
//! gets to its longest (`maxSeconds`, 10 s), the shorter the pause that ends
//! it, 300 ms down to 90 ms. A piece that reaches its longest without one
//! ends at its quietest frame of the last 4 s.

use std::ops::Range;

/// 30 ms at 16 kHz.
pub const FRAME: usize = 480;
pub const RATE: usize = 16_000;
const fn frames(ms: usize) -> usize {
    ms * RATE / 1_000 / FRAME
}
/// Granite does worse on longer audio.
const LONGEST: f64 = 30.0;
/// Where a piece that found no pause is cut: its quietest frame this close
/// to the end.
const FORCED_WINDOW: usize = frames(4_000);
/// The pause that ends a piece of its target length, and of its longest.
const LONG_PAUSE: usize = frames(300);
const SHORT_PAUSE: usize = frames(90);
/// Frames at least this quiet are digital silence (a starting microphone)
/// and say nothing about the room.
const DIGITAL_SILENCE: f32 = -100.0;
/// Quieter frames are silent, whatever else the dictation holds.
const QUIET: f32 = -60.0;
/// Loudness histogram: 1 dB bins from -100 to 0 dBFS.
const BINS: usize = 100;
/// A piece with fewer loud frames than this is not sent to the model.
const SPEECH_FRAMES: usize = 3;

/// How long pieces are, in frames.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    target: usize,
    max: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits::new(5.0, 10.0)
    }
}

impl Limits {
    /// In seconds; the target from 1 s, the longest from the target, both
    /// at most 30 s.
    pub fn new(target: f64, max: f64) -> Limits {
        let target = target.clamp(1.0, LONGEST);
        let max = max.clamp(target, LONGEST);
        let frames = |s: f64| (s * RATE as f64 / FRAME as f64).round() as usize;
        Limits {
            target: frames(target),
            max: frames(max),
        }
    }

    /// The longest piece, in samples.
    pub fn max_samples(&self) -> usize {
        self.max * FRAME
    }

    /// The longest request when pieces piled up, in samples: no longer than
    /// one piece of the target length, so they do not go together again.
    pub fn merge_samples(&self) -> usize {
        self.target * FRAME
    }

    /// The pause (in frames) that ends a piece `len` frames long, or None
    /// if it is too short to end.
    fn pause_needed(&self, len: usize) -> Option<usize> {
        if len < self.target {
            return None;
        }
        let span = (self.max - self.target).max(1);
        let past = (len - self.target).min(span);
        Some(LONG_PAUSE - (LONG_PAUSE - SHORT_PAUSE) * past / span)
    }
}

/// Loudness of one frame in dBFS.
fn level(samples: &[i16]) -> f32 {
    let sum: f64 = samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    let mean = sum / samples.len().max(1) as f64 / (32_768.0 * 32_768.0);
    if mean <= 0.0 {
        -200.0
    } else {
        (10.0 * mean.log10()) as f32
    }
}

#[derive(Default)]
pub struct Segmenter {
    limits: Limits,
    /// Loudness of every whole frame so far.
    levels: Vec<f32>,
    /// Samples that do not make a whole frame yet.
    partial: Vec<i16>,
    histogram: Vec<u32>,
    counted: u32,
    /// Where pieces end, in samples, in order.
    cuts: Vec<usize>,
    /// First frame of the piece being heard.
    start: usize,
    /// First frame of the silence before the newest frame, if any.
    pause: Option<usize>,
}

impl Segmenter {
    pub fn new(limits: Limits) -> Segmenter {
        Segmenter {
            limits,
            histogram: vec![0; BINS],
            ..Segmenter::default()
        }
    }

    /// Where the pieces heard so far end, in samples.
    pub fn cuts(&self) -> &[usize] {
        &self.cuts
    }

    /// The level below which a frame counts as silent. With little range
    /// between floor and speech (someone talking from the first moment) it
    /// stays below the speech.
    fn threshold(&self) -> f32 {
        if self.counted == 0 {
            return QUIET;
        }
        let percentile = |p: f64| {
            let wanted = (f64::from(self.counted) * p).ceil().max(1.0) as u32;
            let mut seen = 0;
            for (bin, &n) in self.histogram.iter().enumerate() {
                seen += n;
                if seen >= wanted {
                    return bin as f32 - BINS as f32;
                }
            }
            0.0
        };
        let (floor, loud) = (percentile(0.1), percentile(0.9));
        let between = floor + ((loud - floor) / 2.0).clamp(6.0, 15.0);
        between.min(loud - 3.0).max(QUIET)
    }

    pub fn push(&mut self, samples: &[i16]) {
        self.partial.extend_from_slice(samples);
        let whole = self.partial.len() / FRAME * FRAME;
        let partial = std::mem::take(&mut self.partial);
        for frame in partial[..whole].chunks(FRAME) {
            self.frame(level(frame));
        }
        self.partial = partial[whole..].to_vec();
    }

    fn frame(&mut self, db: f32) {
        let i = self.levels.len();
        self.levels.push(db);
        if db > DIGITAL_SILENCE {
            let bin = ((db + BINS as f32) as usize).min(BINS - 1);
            self.histogram[bin] += 1;
            self.counted += 1;
        }
        if db < self.threshold() {
            self.pause.get_or_insert(i);
        } else {
            self.pause = None;
        }
        if let Some(p) = self.pause {
            let run = i + 1 - p;
            let middle = p + run / 2;
            if self
                .limits
                .pause_needed(middle - self.start)
                .is_some_and(|needed| run >= needed)
            {
                self.cut(middle);
                // The rest of the pause starts the next piece.
                self.pause = Some(middle);
                return;
            }
        }
        if i + 1 - self.start >= self.limits.max {
            let from = i + 1 - FORCED_WINDOW.min(self.limits.max - 1);
            let quietest = (from..=i)
                .min_by(|&a, &b| self.levels[a].total_cmp(&self.levels[b]))
                .unwrap_or(i);
            self.cut(quietest.max(self.start + 1));
            self.pause = None;
        }
    }

    fn cut(&mut self, frame: usize) {
        self.cuts.push(frame * FRAME);
        self.start = frame;
    }

    /// The dictation is over: whatever follows the last cut is the last piece.
    pub fn finish(&mut self, total: usize) {
        if total > self.cuts.last().copied().unwrap_or(0) {
            self.cuts.push(total);
        }
    }

    /// Whether `range` (samples) holds anything but silence.
    pub fn has_speech(&self, range: Range<usize>) -> bool {
        let threshold = self.threshold();
        let frames = range.start / FRAME..range.end.div_ceil(FRAME).min(self.levels.len());
        self.levels[frames]
            .iter()
            .filter(|&&db| db >= threshold)
            .count()
            >= SPEECH_FRAMES
    }
}

/// What to transcribe next: from `done` up to the furthest cut that keeps it
/// within `max` samples (at least up to the next cut). Pieces that piled up
/// while the model was busy or loading go together, which is fewer requests
/// and more context for each.
pub fn next_job(cuts: &[usize], done: usize, max: usize) -> Option<Range<usize>> {
    let mut end = None;
    for &cut in cuts.iter().filter(|&&c| c > done) {
        if end.is_some() && cut - done > max {
            break;
        }
        end = Some(cut);
    }
    end.map(|end| done..end)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Speech-like sound (a loud 300 Hz tone) or room noise, by milliseconds.
    pub enum Part {
        Speech(usize),
        Quiet(usize),
    }
    use Part::{Quiet, Speech};

    pub fn audio(parts: &[Part]) -> Vec<i16> {
        let mut out = Vec::new();
        for part in parts {
            let (ms, amplitude) = match part {
                Speech(ms) => (*ms, 8_000.0),
                Quiet(ms) => (*ms, 30.0),
            };
            for _ in 0..ms * RATE / 1_000 {
                let t = out.len() as f64 / RATE as f64;
                out.push(((2.0 * std::f64::consts::PI * 300.0 * t).sin() * amplitude) as i16);
            }
        }
        out
    }

    fn cuts_with(limits: Limits, samples: &[i16]) -> Vec<usize> {
        let mut s = Segmenter::new(limits);
        s.push(samples);
        s.finish(samples.len());
        s.cuts().to_vec()
    }

    /// With the default 5 s target and 10 s longest.
    fn cuts_of(samples: &[i16]) -> Vec<usize> {
        cuts_with(Limits::default(), samples)
    }

    fn ms(samples: usize) -> usize {
        samples * 1_000 / RATE
    }

    #[test]
    fn a_pause_after_the_target_ends_a_piece_in_its_middle() {
        let a = audio(&[Speech(6_000), Quiet(700), Speech(3_000)]);
        let cuts = cuts_of(&a);
        assert_eq!(cuts.len(), 2, "{cuts:?}");
        // Cut once 270 ms of the pause were heard, in the middle of those.
        assert!((6_000..=6_300).contains(&ms(cuts[0])), "{}", ms(cuts[0]));
        assert_eq!(cuts[1], a.len());
    }

    #[test]
    fn nothing_is_cut_before_the_target() {
        let a = audio(&[Speech(3_000), Quiet(700), Speech(3_000)]);
        assert_eq!(cuts_of(&a), vec![a.len()]);
    }

    #[test]
    fn a_short_pause_ends_only_a_piece_near_its_longest() {
        // 200 ms is not enough at 5.5 s (300 ms), but is at 9 s (150 ms).
        let a = audio(&[
            Speech(5_500),
            Quiet(200),
            Speech(3_300),
            Quiet(200),
            Speech(2_000),
        ]);
        let cuts = cuts_of(&a);
        assert_eq!(cuts.len(), 2, "{cuts:?}");
        assert!((9_000..=9_200).contains(&ms(cuts[0])), "{}", ms(cuts[0]));
    }

    #[test]
    fn speech_without_pauses_is_cut_at_its_longest_at_its_quietest() {
        // A slight dip at 8 s, no real pause anywhere.
        let mut a = audio(&[Speech(8_000)]);
        let dip = a.len();
        a.extend(audio(&[Speech(60)]).iter().map(|s| s / 4));
        a.extend(audio(&[Speech(22_000)]));
        let cuts = cuts_of(&a);
        assert!((ms(dip)..=ms(dip) + 60).contains(&ms(cuts[0])), "{cuts:?}");
        let mut start = 0;
        for &c in &cuts {
            assert!(c - start <= 10 * RATE, "{} ms", ms(c - start));
            start = c;
        }
        assert_eq!(*cuts.last().unwrap(), a.len());
        assert!(cuts.len() >= 4, "{cuts:?}");
    }

    #[test]
    fn longer_targets_make_longer_pieces() {
        let a = audio(&[
            Speech(6_000),
            Quiet(400),
            Speech(6_000),
            Quiet(400),
            Speech(6_000),
            Quiet(400),
            Speech(6_000),
        ]);
        assert_eq!(cuts_of(&a).len(), 4);
        assert_eq!(cuts_with(Limits::new(10.0, 20.0), &a).len(), 2);
        assert_eq!(cuts_with(Limits::new(30.0, 30.0), &a), vec![a.len()]);
    }

    #[test]
    fn limits_stay_within_what_granite_takes() {
        // Whole 30 ms frames.
        assert_eq!(Limits::new(5.0, 10.0).max_samples(), 333 * FRAME);
        assert_eq!(Limits::new(5.0, 10.0).merge_samples(), 167 * FRAME);
        let tiny = Limits::new(0.2, 100.0);
        assert_eq!((tiny.target, tiny.max), (33, 1_000));
        let crossed = Limits::new(8.0, 4.0);
        assert_eq!(crossed.max, crossed.target);
    }

    #[test]
    fn cutting_does_not_depend_on_how_the_audio_arrives() {
        let a = audio(&[
            Quiet(500),
            Speech(5_000),
            Quiet(600),
            Speech(5_000),
            Quiet(350),
            Speech(6_000),
        ]);
        let whole = cuts_of(&a);
        let mut s = Segmenter::new(Limits::default());
        for piece in a.chunks(337) {
            s.push(piece);
        }
        s.finish(a.len());
        assert_eq!(s.cuts(), &whole[..]);
        assert_eq!(whole.len(), 3, "{whole:?}");
    }

    #[test]
    fn silence_is_not_speech() {
        let a = audio(&[Quiet(5_000)]);
        let mut s = Segmenter::new(Limits::default());
        s.push(&a);
        s.finish(a.len());
        assert!(!s.has_speech(0..a.len()));
        // Nor is a microphone that only delivers zeros.
        let mut z = Segmenter::new(Limits::default());
        z.push(&vec![0i16; 48_000]);
        assert!(!z.has_speech(0..48_000));
    }

    #[test]
    fn a_long_silent_start_is_a_piece_of_its_own_without_speech() {
        let a = audio(&[Quiet(11_000), Speech(2_000)]);
        let mut s = Segmenter::new(Limits::default());
        s.push(&a);
        s.finish(a.len());
        let cuts = s.cuts().to_vec();
        assert_eq!(cuts.len(), 2, "{cuts:?}");
        assert!(!s.has_speech(0..cuts[0]));
        assert!(s.has_speech(cuts[0]..cuts[1]));
    }

    #[test]
    fn pieces_that_piled_up_go_together_up_to_the_limit() {
        let cuts = [100, 250, 400, 900];
        assert_eq!(next_job(&cuts, 0, 500), Some(0..400));
        assert_eq!(next_job(&cuts, 400, 500), Some(400..900));
        assert_eq!(next_job(&cuts, 900, 500), None);
        // A piece longer than the limit still goes, alone.
        assert_eq!(next_job(&[700], 0, 500), Some(0..700));
        assert_eq!(next_job(&[], 0, 500), None);
    }
}
