//! Microphone capture: runs a capture command that writes raw 16-bit mono
//! PCM at 16 kHz to stdout (PipeWire's `pw-cat --record --raw` by default)
//! and cuts it into the 40 ms chunks Wispr's Hub sends.

use std::io::Read;
use std::process::{Child, Command as Process, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;

use crate::protocol::{Event, Timing};

/// Samples per chunk: Wispr's recorder worklet uses 640 at 16 kHz.
pub const CHUNK_SAMPLES: usize = 640;
pub const CHUNK_BYTES: usize = CHUNK_SAMPLES * 2;
pub const SAMPLE_RATE: f64 = 16_000.0;
/// How long the capture command may take to deliver its first audio.
pub const FIRST_AUDIO_PATIENCE: Duration = Duration::from_millis(2_500);

/// Loudness of a chunk in dBFS, the way Wispr reports it; -200 for silence.
pub fn volume_db(pcm: &[u8]) -> f64 {
    let samples = pcm.len() / 2;
    if samples == 0 {
        return -200.0;
    }
    let sum: f64 = pcm
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| {
            let s = f64::from(i16::from_le_bytes(*b)) / 32_768.0;
            s * s
        })
        .sum();
    let rms = (sum / samples as f64).sqrt();
    if rms <= 0.0 {
        -200.0
    } else {
        (20.0 * rms.log10()).max(-200.0)
    }
}

/// Duration of `bytes` of PCM, in milliseconds.
pub fn duration_ms(bytes: usize) -> f64 {
    (bytes / 2) as f64 / SAMPLE_RATE * 1_000.0
}

/// Cuts a byte stream into whole chunks; keeps the remainder for later.
#[derive(Default)]
pub struct Chunker {
    pending: Vec<u8>,
}

impl Chunker {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        self.pending.extend_from_slice(bytes);
        let mut out = Vec::new();
        while self.pending.len() >= CHUNK_BYTES {
            let rest = self.pending.split_off(CHUNK_BYTES);
            out.push(std::mem::replace(&mut self.pending, rest));
        }
        out
    }

    /// What is left at the end of a recording, whole samples only.
    pub fn finish(&mut self) -> Option<Vec<u8>> {
        let even = self.pending.len() & !1;
        self.pending.truncate(even);
        (!self.pending.is_empty()).then(|| std::mem::take(&mut self.pending))
    }
}

fn epoch_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1_000.0)
        .unwrap_or(0.0)
}

/// Where a recording's audio comes from.
enum Source {
    /// A capture command and the thread reading its stdout.
    Process {
        child: Child,
        reader: Option<JoinHandle<()>>,
    },
    /// macOS: the daemon's prepared input unit, running for this recording.
    #[cfg(target_os = "macos")]
    Native(std::sync::Arc<crate::mic_mac::Prepared>),
}

impl Source {
    /// Stops capture; the byte channel closes once the rest is delivered.
    fn stop(&mut self) {
        match self {
            Source::Process { child, reader } => {
                let _ = child.kill();
                let _ = child.wait();
                if let Some(reader) = reader.take() {
                    let _ = reader.join();
                }
            }
            #[cfg(target_os = "macos")]
            Source::Native(capture) => capture.stop(),
        }
    }
}

/// One recording in progress.
pub struct Recording {
    uuid: String,
    source: Source,
    bytes: Receiver<Vec<u8>>,
    chunker: Chunker,
    index: u64,
    started: Instant,
    /// Milliseconds since the process started, for Wispr's start/stop times.
    started_at: f64,
    first_chunk: Option<Instant>,
    device: String,
}

/// What a recording produced since the last look.
pub enum Progress {
    Chunks(Vec<Event>),
    Failed(Event),
}

impl Recording {
    /// Starts the capture command. `clock_ms` is the time since this process
    /// started, which Wispr's Hub reports as performance.now().
    pub fn start(
        uuid: String,
        capture: &[String],
        device: String,
        clock_ms: f64,
    ) -> Result<Recording, String> {
        let (program, args) = capture.split_first().ok_or("no capture command")?;
        let mut child = Process::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("cannot run {program}: {e}"))?;
        let mut stdout = child.stdout.take().ok_or("capture command has no stdout")?;
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut buf = vec![0u8; CHUNK_BYTES];
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Ok(Recording {
            uuid,
            source: Source::Process {
                child,
                reader: Some(reader),
            },
            bytes: rx,
            chunker: Chunker::default(),
            index: 0,
            started: Instant::now(),
            started_at: clock_ms,
            first_chunk: None,
            device,
        })
    }

    /// macOS: records with the daemon's prepared input unit, which starts in
    /// a few milliseconds.
    #[cfg(target_os = "macos")]
    pub fn start_native(
        uuid: String,
        unit: std::sync::Arc<crate::mic_mac::Prepared>,
        device: String,
        clock_ms: f64,
    ) -> Result<Recording, String> {
        let (tx, rx) = mpsc::channel();
        unit.start(tx)?;
        Ok(Recording {
            uuid,
            source: Source::Native(unit),
            bytes: rx,
            chunker: Chunker::default(),
            index: 0,
            started: Instant::now(),
            started_at: clock_ms,
            first_chunk: None,
            device,
        })
    }

    pub fn uuid(&self) -> &str {
        &self.uuid
    }

    fn chunk_event(&mut self, pcm: &[u8]) -> Event {
        if self.first_chunk.is_none() {
            self.first_chunk = Some(Instant::now());
        }
        let event = Event::Chunk {
            uuid: self.uuid.clone(),
            index: self.index,
            volume: volume_db(pcm),
            duration: duration_ms(pcm.len()),
            pcm: base64::engine::general_purpose::STANDARD.encode(pcm),
        };
        self.index += 1;
        event
    }

    /// Waits up to `wait` for audio and returns the chunks that are complete.
    pub fn poll(&mut self, wait: Duration) -> Progress {
        let mut events = Vec::new();
        match self.bytes.recv_timeout(wait) {
            Ok(first) => {
                let mut data = first;
                while let Ok(more) = self.bytes.try_recv() {
                    data.extend_from_slice(&more);
                }
                for chunk in self.chunker.push(&data) {
                    events.push(self.chunk_event(&chunk));
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                if self.first_chunk.is_none() && self.chunker.pending.is_empty() {
                    return Progress::Failed(
                        self.failure("the capture command ended without audio"),
                    );
                }
            }
        }
        if self.first_chunk.is_none() && self.started.elapsed() > FIRST_AUDIO_PATIENCE {
            return Progress::Failed(self.failure("no audio from the microphone"));
        }
        Progress::Chunks(events)
    }

    fn failure(&mut self, why: &str) -> Event {
        self.source.stop();
        Event::MicFailed {
            uuid: self.uuid.clone(),
            error: why.to_string(),
        }
    }

    /// Ends the recording: stops capture, sends what is left, then reports.
    pub fn stop(mut self, clock_ms: f64) -> Vec<Event> {
        let stop_at = Instant::now();
        self.source.stop();
        let mut events = Vec::new();
        let mut rest = Vec::new();
        while let Ok(more) = self.bytes.try_recv() {
            rest.extend_from_slice(&more);
        }
        for chunk in self.chunker.push(&rest) {
            events.push(self.chunk_event(&chunk));
        }
        if let Some(tail) = self.chunker.finish() {
            events.push(self.chunk_event(&tail));
        }
        let ms = |d: Duration| d.as_secs_f64() * 1_000.0;
        let first = self
            .first_chunk
            .map(|f| ms(f.duration_since(self.started)))
            .unwrap_or(-1.0);
        let timing = Timing {
            start_time: self.started_at,
            stop_time: clock_ms,
            absolute_stop_time: epoch_ms(),
            audio_device_name: self.device.clone(),
            mic_acquisition_succeeded: self.first_chunk.is_some(),
            media_stream_setup: first.max(0.0),
            worklet_setup: 0.0,
            first_audio_chunk_latency: first,
            recording_time: ms(stop_at.duration_since(self.started)),
            time_to_receive_last_audio_chunk: ms(stop_at.elapsed()),
            recording_finish: ms(stop_at.elapsed()),
        };
        events.push(Event::Done {
            uuid: self.uuid.clone(),
            timing,
        });
        events
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        self.source.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(samples: usize, amplitude: i16) -> Vec<u8> {
        (0..samples)
            .flat_map(|i| (if i % 2 == 0 { amplitude } else { -amplitude }).to_le_bytes())
            .collect()
    }

    #[test]
    fn silence_is_minus_200() {
        assert_eq!(volume_db(&[0u8; 1280]), -200.0);
        assert_eq!(volume_db(&[]), -200.0);
    }

    #[test]
    fn full_scale_square_is_about_zero_db() {
        let v = volume_db(&tone(640, i16::MAX));
        assert!(v > -0.1 && v <= 0.0, "{v}");
    }

    #[test]
    fn half_scale_is_about_minus_6_db() {
        let v = volume_db(&tone(640, 16_384));
        assert!((v + 6.02).abs() < 0.05, "{v}");
    }

    #[test]
    fn chunk_is_40_ms() {
        assert_eq!(duration_ms(CHUNK_BYTES), 40.0);
    }

    #[test]
    fn chunker_cuts_whole_chunks_and_keeps_the_rest() {
        let mut c = Chunker::default();
        assert!(c.push(&[1u8; 1000]).is_empty());
        let out = c.push(&[2u8; 2000]);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|ch| ch.len() == CHUNK_BYTES));
        assert_eq!(out[0][999], 1);
        assert_eq!(out[0][1000], 2);
        let tail = c.finish().unwrap();
        assert_eq!(tail.len(), 3000 - 2 * CHUNK_BYTES);
        assert!(c.finish().is_none());
    }

    #[test]
    fn chunker_drops_a_half_sample_at_the_end() {
        let mut c = Chunker::default();
        c.push(&[0u8; 5]);
        assert_eq!(c.finish().unwrap().len(), 4);
    }
}
