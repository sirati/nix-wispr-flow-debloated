//! Converts captured audio to Wispr's format, 16 kHz mono 16-bit, from
//! whatever rate the device runs at (48 kHz for a Mac's microphone, 16 or
//! 24 kHz for Bluetooth headsets). A streaming windowed-sinc resampler: each
//! output sample is a weighted sum of the input around its position, with the
//! filter cut below the lower Nyquist frequency so nothing aliases.

// Only the macOS recorder uses it so far.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

/// Taps on each side of an output position.
const HALF_TAPS: usize = 12;

pub struct Resampler {
    /// Input samples per output sample.
    step: f64,
    /// Filter cutoff as a fraction of the input Nyquist frequency.
    cutoff: f64,
    /// Input not yet fully used, starting HALF_TAPS samples of history back.
    buffer: Vec<f32>,
    /// Position of the next output sample in `buffer`.
    pos: f64,
}

impl Resampler {
    pub fn new(from_hz: f64, to_hz: f64) -> Resampler {
        let step = from_hz / to_hz;
        Resampler {
            step,
            // Below the output's Nyquist frequency when downsampling.
            cutoff: (1.0 / step).min(1.0) * 0.92,
            buffer: vec![0.0; HALF_TAPS],
            pos: HALF_TAPS as f64,
        }
    }

    /// Starts over, as for a new recording.
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.buffer.resize(HALF_TAPS, 0.0);
        self.pos = HALF_TAPS as f64;
    }

    fn weight(&self, t: f64) -> f64 {
        let x = t * self.cutoff;
        let sinc = if x.abs() < 1e-9 {
            1.0
        } else {
            (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
        };
        // Blackman window over the filter's span.
        let w = (t / HALF_TAPS as f64 + 1.0) / 2.0;
        let blackman = if (0.0..=1.0).contains(&w) {
            0.42 - 0.5 * (2.0 * std::f64::consts::PI * w).cos()
                + 0.08 * (4.0 * std::f64::consts::PI * w).cos()
        } else {
            0.0
        };
        self.cutoff * sinc * blackman
    }

    /// Takes `input` at the source rate and returns whatever output samples
    /// are complete, as 16-bit little-endian PCM bytes.
    pub fn push(&mut self, input: &[f32]) -> Vec<u8> {
        self.buffer.extend_from_slice(input);
        let mut out = Vec::new();
        loop {
            let centre = self.pos.floor() as usize;
            if centre + HALF_TAPS >= self.buffer.len() {
                break;
            }
            let frac = self.pos - centre as f64;
            let mut acc = 0.0f64;
            for k in 0..2 * HALF_TAPS {
                let i = centre + k + 1 - HALF_TAPS;
                let t = (k as f64 + 1.0 - HALF_TAPS as f64) - frac;
                acc += f64::from(self.buffer[i]) * self.weight(t);
            }
            let sample = (acc * 32767.0).round().clamp(-32768.0, 32767.0) as i16;
            out.extend_from_slice(&sample.to_le_bytes());
            self.pos += self.step;
        }
        // Keep what the next output still needs.
        let keep_from = (self.pos.floor() as usize).saturating_sub(HALF_TAPS - 1);
        if keep_from > 0 {
            self.buffer.drain(..keep_from.min(self.buffer.len()));
            self.pos -= keep_from as f64;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(hz: f64, rate: f64, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (0.5 * (2.0 * std::f64::consts::PI * hz * i as f64 / rate).sin()) as f32)
            .collect()
    }

    fn samples(bytes: &[u8]) -> Vec<f64> {
        bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| f64::from(i16::from_le_bytes(*b)) / 32767.0)
            .collect()
    }

    fn rms(x: &[f64]) -> f64 {
        (x.iter().map(|v| v * v).sum::<f64>() / x.len() as f64).sqrt()
    }

    #[test]
    fn makes_a_third_of_the_samples_from_48_khz() {
        let mut r = Resampler::new(48_000.0, 16_000.0);
        let mut n = 0;
        // In uneven pieces, as a device delivers them.
        for piece in sine(440.0, 48_000.0, 48_000).chunks(471) {
            n += r.push(piece).len() / 2;
        }
        assert!((15_990..=16_000).contains(&n), "{n}");
    }

    #[test]
    fn speech_frequencies_pass_at_full_level() {
        let mut r = Resampler::new(48_000.0, 16_000.0);
        let out = samples(&r.push(&sine(1_000.0, 48_000.0, 48_000)));
        let level = rms(&out[200..]);
        assert!((level - 0.5 / 2f64.sqrt()).abs() < 0.01, "{level}");
    }

    #[test]
    fn frequencies_above_8_khz_do_not_fold_back() {
        let mut r = Resampler::new(48_000.0, 16_000.0);
        let out = samples(&r.push(&sine(12_000.0, 48_000.0, 48_000)));
        assert!(rms(&out[200..]) < 0.01, "{}", rms(&out[200..]));
    }

    #[test]
    fn same_rate_keeps_the_signal_and_upsampling_works() {
        let mut same = Resampler::new(16_000.0, 16_000.0);
        let out = samples(&same.push(&sine(500.0, 16_000.0, 16_000)));
        assert!((rms(&out[200..]) - 0.5 / 2f64.sqrt()).abs() < 0.01);
        let mut up = Resampler::new(8_000.0, 16_000.0);
        let n = up.push(&sine(500.0, 8_000.0, 8_000)).len() / 2;
        assert!((15_970..=16_000).contains(&n), "{n}");
    }

    #[test]
    fn reset_starts_a_new_recording_cleanly() {
        let mut r = Resampler::new(48_000.0, 16_000.0);
        r.push(&sine(440.0, 48_000.0, 1_000));
        r.reset();
        let out = r.push(&[0.0; 48]);
        assert!(samples(&out).iter().all(|s| s.abs() < 1e-4));
    }
}
