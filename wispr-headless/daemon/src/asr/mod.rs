//! Dictation transcribed on this machine by IBM's Granite Speech (through
//! llama.cpp) instead of Wispr's service.
//!
//! Nothing stays in memory between dictations. When one starts, llama-server
//! starts with the model, encoder and language model together (server.rs);
//! the audio is buffered meanwhile (the load takes about 3 s on the Mac).
//! The server stops as soon as the text is out, unless the next dictation
//! already waits for it (Slot) or `idleSeconds` says to keep it.
//!
//! Granite does not stream, so a dictation is cut into pieces of about 5 s
//! at its pauses (segment.rs) and each piece is transcribed once it is
//! complete. At about 4x realtime that catches up with the speaker after the
//! load, and when the dictation ends only its last few seconds are left. The
//! texts are joined with spaces.
//!
//! With `cleanup` set, the joined text is cleaned up by a second, smaller
//! model in a llama-server of its own (cleanup.rs). It starts loading when
//! the dictation ends, while the last piece is transcribed; Granite's server
//! stops as soon as that piece is done (Granite and the cleanup model are
//! both in memory only for that while), and the cleanup server after its
//! answer, `idleSeconds` again keeping either for the next dictation. If
//! the cleanup fails or its answer looks wrong, the text is typed as Granite
//! wrote it.
//!
//! Use: `Asr::start` when a dictation starts, `Dictation::feed` with its
//! audio as it arrives, `Dictation::finish` for the text when it ends.
//! Dropping an unfinished Dictation cancels it. `Dictation::failure` tells
//! early that it failed (the server died while the user still speaks).

mod cleanup;
mod prompt;
mod segment;
mod server;

use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;

use cleanup::{CleanupSettings, Pending};
use segment::{next_job, Limits, Segmenter, RATE};
use server::Server;

/// Where the model files are unless the settings say otherwise.
#[cfg(target_os = "macos")]
const MODEL_DIR: &str = "~/Library/Application Support/wispr-headless";
#[cfg(not(target_os = "macos"))]
const MODEL_DIR: &str = "~/.local/share/wispr-headless";

/// The `--asr-config` file (JSON). Paths may start with `~/`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Settings {
    /// llama.cpp's llama-server, b11852 or later (default: from PATH).
    #[serde(default = "default_server")]
    pub server: String,
    /// Granite Speech's language model.
    #[serde(default = "default_model")]
    pub model: String,
    /// Its audio encoder. The default is quantized to Q4_K except its
    /// depthwise convolutions (`llama-quantize --tensor-type conv_dw=f32`):
    /// 318 MB instead of 1.2 GB at F16.
    #[serde(default = "default_mmproj")]
    pub mmproj: String,
    /// Run the audio encoder on Core ML (the Neural Engine) instead, with
    /// mmproj only for what Core ML declines (macOS, with llama.cpp's
    /// patch 0003).
    #[serde(default)]
    pub coreml: Option<CoreMl>,
    /// Terms the text should spell as listed, one per line (prompt.rs).
    /// Read when a dictation starts, so edits apply to the next one.
    #[serde(default)]
    pub keywords: Option<String>,
    /// How long the models stay loaded after a dictation, for the next.
    #[serde(default)]
    pub idle_seconds: f64,
    /// More arguments for llama-server.
    #[serde(default)]
    pub server_args: Vec<String>,
    /// How long pieces are (segment.rs): cut at the first pause after
    /// this, so little is left when the dictation ends.
    #[serde(default = "default_target")]
    pub target_seconds: f64,
    /// And cut here, pause or not.
    #[serde(default = "default_max")]
    pub max_seconds: f64,
    /// Clean up each transcript with a second model (cleanup.rs); without
    /// this, it is typed as Granite wrote it.
    #[serde(default)]
    pub cleanup: Option<CleanupSettings>,
}

/// `Settings.coreml`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CoreMl {
    /// A granite_speech_audio_*.mlpackage (compiled on first use into
    /// ~/Library/Caches/llama-mtmd-coreml).
    pub encoder: String,
    /// Which of its functions to load, by length in frames of 20 ms: 250,
    /// 375 and 500 (5, 7.5 and 10 s). Each piece pads to the smallest that
    /// holds it. Empty loads only 500, to avoid several loaded copies of
    /// the encoder. List [250, 375, 500] to load all three.
    #[serde(default)]
    pub frames: Vec<u32>,
}

fn default_server() -> String {
    "llama-server".into()
}

fn default_model() -> String {
    format!("{MODEL_DIR}/granite-speech-5.0-2b-Q4_K_M.gguf")
}

fn default_mmproj() -> String {
    format!("{MODEL_DIR}/mmproj-model-q4_k-convdw-f32.gguf")
}

fn default_target() -> f64 {
    5.0
}

fn default_max() -> f64 {
    10.0
}

impl Settings {
    pub fn load(path: &str) -> Result<Settings, String> {
        let text = std::fs::read_to_string(server::expand(path))
            .map_err(|e| format!("cannot read {path}: {e}"))?;
        serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))
    }

    fn limits(&self) -> Limits {
        Limits::new(self.target_seconds, self.max_seconds)
    }
}

type Log = Arc<dyn Fn(String) + Send + Sync>;

/// How often a dictation waiting for audio checks that the server is up.
const CHECK_EVERY: Duration = Duration::from_millis(250);

/// Transcribes the pieces of a dictation: llama-server, or a stand-in in
/// tests.
trait Engine: Send + 'static {
    /// Processes the prompt while no piece is ready yet. The server keeps it
    /// cached for the pieces, so a long keyword list is read once, not once
    /// a piece.
    fn prime(&mut self) -> Result<(), String>;
    fn transcribe(&mut self, pcm: &[i16]) -> Result<String, String>;
    /// Err with the reason once it can no longer transcribe (the server
    /// died). Asked while waiting for audio.
    fn check(&mut self) -> Result<(), String>;
}

struct Llama {
    lease: Lease,
    prompt: String,
}

impl Llama {
    /// Starts the server (or takes the one the last dictation used) and
    /// waits for the model.
    fn load(
        settings: &Settings,
        slot: &Arc<Slot>,
        log: &Log,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Llama, String> {
        let mut keywords = match &settings.keywords {
            None => Vec::new(),
            Some(path) => match std::fs::read_to_string(server::expand(path)) {
                Ok(text) => prompt::parse_keywords(&text),
                Err(e) => {
                    log(format!("no keywords: cannot read {path}: {e}"));
                    Vec::new()
                }
            },
        };
        let all = keywords.len();
        let longest = settings.limits().max_samples();
        keywords.truncate(prompt::fit_estimate(&keywords, longest).len());
        let context = prompt::context_for(&prompt::prompt(&keywords), longest);
        let mut lease = slot.take(settings, cancelled)?;
        // A kept server may have died meanwhile.
        if !lease
            .server
            .as_mut()
            .is_some_and(|s| s.context >= context && s.ended().is_none())
        {
            lease.server = None;
            lease.server = Some(Server::spawn(settings, context, &**log)?);
        }
        let server = lease.server.as_mut().expect("just started");
        server.ready(cancelled)?;
        // The estimate errs high, but names and numbers can take more: count,
        // and drop keywords from the end until they fit.
        let budget = server.context - prompt::piece_tokens(longest);
        loop {
            let tokens = server.tokens(&prompt::prompt(&keywords))?;
            if tokens <= budget || keywords.is_empty() {
                break;
            }
            let keep = prompt::keep_for(keywords.len(), tokens, budget);
            keywords.truncate(keep.min(keywords.len() - 1));
        }
        if keywords.len() < all {
            log(format!(
                "only {} of {all} keywords fit the model's context",
                keywords.len()
            ));
        }
        lease.healthy = true;
        Ok(Llama {
            lease,
            prompt: prompt::prompt(&keywords),
        })
    }

    /// Asks the server; a server that failed once is not used again.
    fn ask(&mut self, pcm: &[i16], max_tokens: u32) -> Result<String, String> {
        let server = self.lease.server.as_mut().expect("loaded");
        let text = server.transcribe(pcm, &self.prompt, max_tokens);
        self.lease.healthy &= text.is_ok();
        text
    }
}

impl Engine for Llama {
    fn prime(&mut self) -> Result<(), String> {
        // A tenth of a second of silence; one token of answer.
        self.ask(&[0; RATE / 10], 1).map(drop)
    }

    fn transcribe(&mut self, pcm: &[i16]) -> Result<String, String> {
        self.ask(pcm, prompt::max_tokens(pcm.len()))
    }

    fn check(&mut self) -> Result<(), String> {
        let server = self.lease.server.as_mut().expect("loaded");
        match server.ended() {
            None => Ok(()),
            Some(why) => {
                self.lease.healthy = false;
                Err(why)
            }
        }
    }
}

/// The one llama-server, so two dictations close together never load the
/// model twice: the second waits for the first to be done with it and takes
/// it over. Without a dictation waiting, a server stays `idleSeconds`.
#[derive(Default)]
struct Slot {
    state: Mutex<SlotState>,
    /// A dictation is done with the server.
    freed: Condvar,
}

#[derive(Default)]
struct SlotState {
    server: Option<Server>,
    /// A dictation has the server (or is starting one).
    busy: bool,
    waiting: usize,
    /// Counts takes, so an idle timer knows whether its server was taken.
    generation: u64,
}

/// A dictation's turn with the server; dropping it ends the turn.
struct Lease {
    slot: Arc<Slot>,
    server: Option<Server>,
    /// Loaded and answering: worth keeping for the next dictation.
    healthy: bool,
    idle: Duration,
}

impl Slot {
    /// Waits for the turn of a dictation that is not cancelled meanwhile.
    fn take(
        self: &Arc<Self>,
        settings: &Settings,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Lease, String> {
        let mut state = self.state.lock().expect("slot lock poisoned");
        state.waiting += 1;
        while state.busy {
            if cancelled() {
                state.waiting -= 1;
                // Nobody else is left for a server kept for this dictation.
                let orphan = (state.waiting == 0).then(|| state.server.take());
                drop(state);
                drop(orphan);
                return Err("cancelled".into());
            }
            state = self
                .freed
                .wait_timeout(state, Duration::from_millis(20))
                .expect("slot lock poisoned")
                .0;
        }
        state.waiting -= 1;
        state.busy = true;
        state.generation += 1;
        Ok(Lease {
            slot: self.clone(),
            server: state.server.take(),
            healthy: false,
            idle: Duration::from_secs_f64(settings.idle_seconds.clamp(0.0, 86_400.0)),
        })
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let server = self.server.take().filter(|_| self.healthy);
        let mut state = self.slot.state.lock().expect("slot lock poisoned");
        state.busy = false;
        let keep = state.waiting > 0 || !self.idle.is_zero();
        if keep && server.is_some() {
            state.server = server;
            if state.waiting == 0 {
                let generation = state.generation;
                let (slot, idle) = (self.slot.clone(), self.idle);
                std::thread::spawn(move || {
                    std::thread::sleep(idle);
                    let mut state = slot.state.lock().expect("slot lock poisoned");
                    if state.generation == generation && !state.busy {
                        state.server = None;
                    }
                });
            }
        } else {
            // Stopped here, with the lock released.
            drop(state);
            drop(server);
        }
        self.slot.freed.notify_all();
    }
}

/// Local transcription, set up once per process; costs nothing until a
/// dictation starts.
pub struct Asr {
    settings: Arc<Settings>,
    slot: Arc<Slot>,
    /// The cleanup model's server, handed on the same way.
    cleanup_slot: Arc<Slot>,
    log: Log,
}

/// Starts the cleanup of a dictation of so many samples, when it ends.
type BeginCleanup = Box<dyn FnOnce(usize) -> Pending + Send>;

impl Asr {
    pub fn new(settings: Settings, log: impl Fn(String) + Send + Sync + 'static) -> Asr {
        Asr {
            settings: Arc::new(settings),
            slot: Arc::default(),
            cleanup_slot: Arc::default(),
            log: Arc::new(log),
        }
    }

    /// A dictation starts: the model starts loading now.
    pub fn start(&self) -> Dictation {
        let (settings, slot, log) = (self.settings.clone(), self.slot.clone(), self.log.clone());
        let cleanup = self.settings.cleanup.is_some().then(|| {
            let (settings, slot, log) = (
                self.settings.clone(),
                self.cleanup_slot.clone(),
                self.log.clone(),
            );
            Box::new(move |samples| Pending::begin(settings, slot, log, samples)) as BeginCleanup
        });
        Dictation::spawn(
            move |cancelled: &dyn Fn() -> bool| Llama::load(&settings, &slot, &log, cancelled),
            self.settings.limits(),
            self.log.clone(),
            cleanup,
        )
    }
}

#[derive(Default)]
struct State {
    /// The whole dictation so far, 16 kHz mono.
    audio: Vec<i16>,
    segmenter: Segmenter,
    /// When it ended (`finish`).
    ended: Option<Instant>,
    cancelled: bool,
}

struct Shared {
    limits: Limits,
    state: Mutex<State>,
    /// New audio, the end, or a cancel.
    wake: Condvar,
}

impl Shared {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("dictation lock poisoned")
    }
}

/// One dictation being transcribed.
pub struct Dictation {
    shared: Arc<Shared>,
    result: Receiver<Result<String, String>>,
    /// What `failure` already took from `result`.
    outcome: Option<Result<String, String>>,
    /// The odd byte of the last feed.
    carry: Option<u8>,
    finished: bool,
    /// Started when it ends, if the text is to be cleaned up.
    cleanup: Option<BeginCleanup>,
}

impl Dictation {
    /// Runs `work` on its own thread. The engine is dropped once the text is
    /// out, which frees the model (or hands it on, see Slot).
    fn spawn<E: Engine>(
        load: impl FnOnce(&dyn Fn() -> bool) -> Result<E, String> + Send + 'static,
        limits: Limits,
        log: Log,
        cleanup: Option<BeginCleanup>,
    ) -> Dictation {
        let shared = Arc::new(Shared {
            limits,
            state: Mutex::new(State {
                segmenter: Segmenter::new(limits),
                ..State::default()
            }),
            wake: Condvar::new(),
        });
        let (tx, result) = mpsc::channel();
        let worker = shared.clone();
        std::thread::spawn(move || {
            let (text, engine) = work(&worker, load, &log);
            // The memory first, for the cleanup model (stopping is quick).
            drop(engine);
            let _ = tx.send(text);
        });
        Dictation {
            shared,
            result,
            outcome: None,
            carry: None,
            finished: false,
            cleanup,
        }
    }

    /// More audio: 16 kHz mono 16-bit little-endian PCM, in pieces of any
    /// size.
    pub fn feed(&mut self, pcm: &[u8]) {
        let mut bytes = Vec::with_capacity(pcm.len() + 1);
        bytes.extend(self.carry.take());
        bytes.extend_from_slice(pcm);
        if bytes.len() % 2 == 1 {
            self.carry = bytes.pop();
        }
        let samples: Vec<i16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b))
            .collect();
        let mut state = self.shared.state();
        state.audio.extend_from_slice(&samples);
        state.segmenter.push(&samples);
        drop(state);
        self.shared.wake.notify_all();
    }

    /// The dictation ended: waits for the rest to be transcribed and returns
    /// the whole text, cleaned up if so configured.
    pub fn finish(mut self) -> Result<String, String> {
        self.finished = true;
        let total = {
            let mut state = self.shared.state();
            state.ended = Some(Instant::now());
            let total = state.audio.len();
            state.segmenter.finish(total);
            total
        };
        self.shared.wake.notify_all();
        // Loads while the last piece is transcribed; dropped (and stopped)
        // if the transcription fails.
        let pending = match self.outcome {
            Some(Err(_)) => None,
            _ => self.cleanup.take().map(|begin| begin(total)),
        };
        let raw = match self.outcome.take() {
            Some(outcome) => outcome,
            None => self.result.recv().unwrap_or_else(|_| Err(stopped())),
        };
        match (raw, pending) {
            (Ok(raw), Some(pending)) => Ok(pending.clean(raw)),
            (raw, _) => raw,
        }
    }

    /// Without waiting: the error the transcription already ended with, if
    /// it did (the server died, say), while audio is still being fed.
    /// `finish` then returns the same.
    pub fn failure(&mut self) -> Option<String> {
        if self.outcome.is_none() {
            self.outcome = match self.result.try_recv() {
                Ok(outcome) => Some(outcome),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => Some(Err(stopped())),
            };
        }
        self.outcome.as_ref()?.as_ref().err().cloned()
    }
}

fn stopped() -> String {
    "the transcription stopped".into()
}

impl Drop for Dictation {
    fn drop(&mut self) {
        if !self.finished {
            self.shared.state().cancelled = true;
            self.shared.wake.notify_all();
        }
    }
}

/// Loads the engine, then transcribes each piece as it is complete. Returns
/// the text, and the engine unless it failed.
fn work<E: Engine>(
    shared: &Shared,
    load: impl FnOnce(&dyn Fn() -> bool) -> Result<E, String>,
    log: &Log,
) -> (Result<String, String>, Option<E>) {
    let begun = Instant::now();
    let mut engine = match load(&|| shared.state().cancelled) {
        Ok(e) => e,
        Err(e) => return (Err(e), None),
    };
    let loaded = begun.elapsed();
    let idle = {
        let state = shared.state();
        !state.cancelled
            && state.ended.is_none()
            && next_job(state.segmenter.cuts(), 0, shared.limits.merge_samples()).is_none()
    };
    if idle {
        if let Err(e) = engine.prime() {
            return (Err(e), None);
        }
    }
    let mut done = 0;
    let mut texts: Vec<String> = Vec::new();
    let mut requests = 0;
    let mut busy = Duration::ZERO;
    let mut checked = Instant::now();
    loop {
        let (job, pcm) = {
            let mut state = shared.state();
            loop {
                if state.cancelled {
                    return (Err("cancelled".into()), Some(engine));
                }
                if let Some(job) =
                    next_job(state.segmenter.cuts(), done, shared.limits.merge_samples())
                {
                    let pcm = state
                        .segmenter
                        .has_speech(job.clone())
                        .then(|| state.audio[job.clone()].to_vec());
                    break (job, pcm);
                }
                if let Some(ended) = state.ended {
                    log(format!(
                        "local transcription: {:.1} s of audio in {requests} requests \
                         taking {:.1} s; model ready after {:.1} s, text {:.2} s after the end",
                        state.audio.len() as f64 / RATE as f64,
                        busy.as_secs_f64(),
                        loaded.as_secs_f64(),
                        ended.elapsed().as_secs_f64()
                    ));
                    return (Ok(texts.join(" ")), Some(engine));
                }
                // Every so often, while audio keeps coming or not: is the
                // engine still there?
                let wait = CHECK_EVERY.saturating_sub(checked.elapsed());
                if wait.is_zero() {
                    drop(state);
                    if let Err(e) = engine.check() {
                        return (Err(e), None);
                    }
                    checked = Instant::now();
                    state = shared.state();
                    continue;
                }
                state = shared
                    .wake
                    .wait_timeout(state, wait)
                    .expect("dictation lock poisoned")
                    .0;
            }
        };
        // Silence is not sent: the model would make words of it.
        if let Some(pcm) = pcm {
            requests += 1;
            let asked = Instant::now();
            let text = engine.transcribe(&pcm);
            busy += asked.elapsed();
            match text {
                Ok(text) if !text.is_empty() => texts.push(text),
                Ok(_) => {}
                Err(e) => return (Err(e), None),
            }
        }
        done = job.end;
    }
}

#[cfg(test)]
mod tests {
    use super::segment::tests::{audio, Part::*};
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn bytes(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    /// What the stand-in engine was asked.
    #[derive(Default)]
    struct Calls {
        primes: usize,
        /// Length of each piece, in samples.
        pieces: Vec<usize>,
        /// The load gave up: the dictation was cancelled.
        load_cancelled: bool,
        dropped: bool,
        /// Set by the test: the engine is gone.
        dead: bool,
        checks: usize,
    }

    struct Fake {
        calls: Arc<Mutex<Calls>>,
        fail: bool,
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            self.calls.lock().unwrap().dropped = true;
        }
    }

    impl Engine for Fake {
        fn prime(&mut self) -> Result<(), String> {
            self.calls.lock().unwrap().primes += 1;
            Ok(())
        }

        fn transcribe(&mut self, pcm: &[i16]) -> Result<String, String> {
            if self.fail {
                return Err("the model fell over".into());
            }
            self.calls.lock().unwrap().pieces.push(pcm.len());
            Ok(format!("[{}ms]", pcm.len() / 16))
        }

        fn check(&mut self) -> Result<(), String> {
            let mut calls = self.calls.lock().unwrap();
            calls.checks += 1;
            match calls.dead {
                true => Err("the model is gone".into()),
                false => Ok(()),
            }
        }
    }

    /// A dictation with the stand-in, which loads once `loaded` says so.
    fn dictation(loaded: Receiver<()>, fail: bool) -> (Dictation, Arc<Mutex<Calls>>) {
        let calls = Arc::new(Mutex::new(Calls::default()));
        let engine = Fake {
            calls: calls.clone(),
            fail,
        };
        let d = Dictation::spawn(
            move |cancelled: &dyn Fn() -> bool| loop {
                if cancelled() {
                    engine.calls.lock().unwrap().load_cancelled = true;
                    return Err("cancelled".to_string());
                }
                if loaded.recv_timeout(Duration::from_millis(5)).is_ok() {
                    return Ok(engine);
                }
            },
            Limits::default(),
            Arc::new(|_| {}),
            None,
        );
        (d, calls)
    }

    fn loaded_at_once() -> Receiver<()> {
        let (tx, rx) = mpsc::channel();
        tx.send(()).unwrap();
        rx
    }

    fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
        for _ in 0..1_000 {
            if ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for {what}");
    }

    #[test]
    fn pieces_are_transcribed_while_the_dictation_goes_on() {
        let (mut d, calls) = dictation(loaded_at_once(), false);
        // Loaded with nothing to do yet: the prompt goes first.
        wait_for("the prompt", || calls.lock().unwrap().primes == 1);
        // Each piece is done as soon as its pause is heard.
        let a = audio(&[Speech(5_000), Quiet(700)]);
        d.feed(&bytes(&a));
        wait_for("the first piece", || {
            calls.lock().unwrap().pieces.len() == 1
        });
        d.feed(&bytes(&a));
        wait_for("the second piece", || {
            calls.lock().unwrap().pieces.len() == 2
        });
        d.feed(&bytes(&audio(&[Speech(1_000)])));
        let text = d.finish().unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls.pieces.len(), 3);
        assert_eq!(calls.pieces.iter().sum::<usize>(), 2 * a.len() + 16_000);
        assert_eq!(text.split(' ').count(), 3, "{text}");
    }

    #[test]
    fn pieces_that_piled_up_while_loading_stay_short() {
        let (load, loaded) = mpsc::channel();
        let (mut d, calls) = dictation(loaded, false);
        let a = audio(&[
            Speech(5_000),
            Quiet(700),
            Speech(5_000),
            Quiet(700),
            Speech(2_000),
        ]);
        d.feed(&bytes(&a));
        std::thread::sleep(Duration::from_millis(50));
        load.send(()).unwrap();
        d.finish().unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls.primes, 0, "pieces were waiting, no need to prime");
        // Not one request of 10 s: no more than the target each.
        assert_eq!(calls.pieces.len(), 3, "{:?}", calls.pieces);
        assert!(calls.pieces[..2].iter().all(|&p| p < 6 * 16_000));
        assert_eq!(calls.pieces.iter().sum::<usize>(), a.len());
    }

    #[test]
    fn a_dictation_that_ends_while_loading_is_still_transcribed() {
        let (load, loaded) = mpsc::channel();
        let (mut d, calls) = dictation(loaded, false);
        d.feed(&bytes(&audio(&[Speech(1_500)])));
        let finished = std::thread::spawn(move || d.finish());
        std::thread::sleep(Duration::from_millis(50));
        load.send(()).unwrap();
        assert_eq!(finished.join().unwrap().unwrap(), "[1500ms]");
        assert_eq!(calls.lock().unwrap().primes, 0);
    }

    #[test]
    fn silence_is_not_sent_to_the_model() {
        let (mut d, calls) = dictation(loaded_at_once(), false);
        d.feed(&bytes(&audio(&[Quiet(8_000)])));
        assert_eq!(d.finish().unwrap(), "");
        assert!(calls.lock().unwrap().pieces.is_empty());
    }

    #[test]
    fn odd_sized_feeds_lose_no_samples() {
        let (mut d, calls) = dictation(loaded_at_once(), false);
        let b = bytes(&audio(&[Speech(2_000)]));
        for piece in b.chunks(333) {
            d.feed(piece);
        }
        d.finish().unwrap();
        assert_eq!(calls.lock().unwrap().pieces, vec![32_000]);
    }

    #[test]
    fn a_failing_model_fails_the_dictation_and_is_freed() {
        let (mut d, calls) = dictation(loaded_at_once(), true);
        d.feed(&bytes(&audio(&[Speech(2_000)])));
        assert_eq!(d.finish().unwrap_err(), "the model fell over");
        wait_for("the model to go", || calls.lock().unwrap().dropped);
    }

    #[test]
    fn a_failed_load_fails_the_dictation() {
        let d = Dictation::spawn(
            |_: &dyn Fn() -> bool| Err::<Fake, _>("no such model".to_string()),
            Limits::default(),
            Arc::new(|_| {}),
            None,
        );
        assert_eq!(d.finish().unwrap_err(), "no such model");
    }

    #[test]
    fn cancelling_stops_the_work_and_frees_the_model() {
        let (mut d, calls) = dictation(loaded_at_once(), false);
        d.feed(&bytes(&audio(&[Speech(2_000)])));
        drop(d);
        wait_for("the model to go", || calls.lock().unwrap().dropped);
        assert!(calls.lock().unwrap().pieces.is_empty());
    }

    #[test]
    fn cancelling_while_loading_stops_the_load() {
        let (_load, loaded) = mpsc::channel();
        let (d, calls) = dictation(loaded, false);
        drop(d);
        wait_for("the load to give up", || {
            calls.lock().unwrap().load_cancelled
        });
    }

    #[test]
    fn a_dead_engine_fails_the_dictation_while_it_records() {
        let (mut d, calls) = dictation(loaded_at_once(), false);
        wait_for("the prompt", || calls.lock().unwrap().primes == 1);
        d.feed(&bytes(&audio(&[Speech(1_000)])));
        assert_eq!(d.failure(), None);
        calls.lock().unwrap().dead = true;
        let start = Instant::now();
        // While audio keeps coming, as it does while the user speaks.
        wait_for("the failure", || {
            d.feed(&bytes(&audio(&[Quiet(20)])));
            d.failure().is_some()
        });
        assert!(start.elapsed() < Duration::from_secs(1), "noticed in time");
        assert_eq!(d.failure().unwrap(), "the model is gone");
        assert_eq!(d.finish().unwrap_err(), "the model is gone");
        assert!(calls.lock().unwrap().dropped);
    }

    #[test]
    fn a_live_dictation_has_no_failure_and_keeps_its_text() {
        let (mut d, calls) = dictation(loaded_at_once(), false);
        d.feed(&bytes(&audio(&[Speech(1_000)])));
        wait_for("a check", || calls.lock().unwrap().checks > 0);
        assert_eq!(d.failure(), None);
        assert_eq!(d.finish().unwrap(), "[1000ms]");
    }

    /// Settings whose server is the shell script `body`.
    fn stand_in(dir: &std::path::Path, body: &str, idle_seconds: f64) -> Settings {
        Settings {
            server: server::tests::script(dir, body)
                .to_string_lossy()
                .into_owned(),
            model: String::new(),
            mmproj: String::new(),
            coreml: None,
            keywords: None,
            idle_seconds,
            server_args: Vec::new(),
            target_seconds: 5.0,
            max_seconds: 10.0,
            cleanup: None,
        }
    }

    /// Settings whose server is a shell script that only waits.
    fn sleeper(dir: &std::path::Path, idle_seconds: f64) -> Settings {
        stand_in(dir, "exec sleep 30", idle_seconds)
    }

    /// A turn with a fresh server, as after a successful load.
    fn loaded_lease(slot: &Arc<Slot>, settings: &Settings) -> (Lease, PathBuf) {
        let mut lease = slot.take(settings, &|| false).unwrap();
        let server = Server::spawn(settings, 1_024, &|_| {}).unwrap();
        let socket = server.socket().to_path_buf();
        lease.server = Some(server);
        lease.healthy = true;
        (lease, socket)
    }

    fn kept(slot: &Slot) -> bool {
        slot.state.lock().unwrap().server.is_some()
    }

    #[test]
    fn a_waiting_dictation_takes_over_the_server() {
        let _one = server::one_at_a_time();
        let dir = tempfile::tempdir().unwrap();
        let settings = sleeper(dir.path(), 0.0);
        let slot = Arc::new(Slot::default());
        let (first, socket) = loaded_lease(&slot, &settings);
        let next = {
            let (slot, settings) = (slot.clone(), settings.clone());
            std::thread::spawn(move || slot.take(&settings, &|| false).unwrap())
        };
        std::thread::sleep(Duration::from_millis(50));
        assert!(!next.is_finished(), "one dictation at a time");
        drop(first);
        let second = next.join().unwrap();
        assert_eq!(second.server.as_ref().unwrap().socket(), socket);
    }

    #[test]
    fn without_a_waiting_dictation_the_server_stops_or_idles() {
        let _one = server::one_at_a_time();
        let dir = tempfile::tempdir().unwrap();
        let slot = Arc::new(Slot::default());
        let (lease, _) = loaded_lease(&slot, &sleeper(dir.path(), 0.0));
        drop(lease);
        assert!(!kept(&slot), "stopped at once");

        let (lease, _) = loaded_lease(&slot, &sleeper(dir.path(), 0.2));
        drop(lease);
        assert!(kept(&slot), "kept for the next dictation");
        wait_for("the idle time to pass", || !kept(&slot));
    }

    #[test]
    fn a_failed_server_is_not_handed_on() {
        let _one = server::one_at_a_time();
        let dir = tempfile::tempdir().unwrap();
        let settings = sleeper(dir.path(), 10.0);
        let slot = Arc::new(Slot::default());
        let (mut lease, _) = loaded_lease(&slot, &settings);
        lease.healthy = false;
        drop(lease);
        assert!(!kept(&slot));
        assert!(slot.take(&settings, &|| false).unwrap().server.is_none());
    }

    /// The pids the stand-in servers wrote to `dir` that still run.
    fn running(dir: &std::path::Path) -> (usize, usize) {
        let pids: Vec<i32> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
            .collect();
        let alive = pids.iter().filter(|&&p| server::tests::alive(p)).count();
        (alive, pids.len())
    }

    #[test]
    fn there_is_never_more_than_one_server() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let _one = server::one_at_a_time();
        let dir = tempfile::tempdir().unwrap();
        let pids = dir.path().join("pids");
        std::fs::create_dir(&pids).unwrap();
        // Never ready, so each dictation is still loading when it goes.
        let body = format!("echo $$ > '{}'/$$\nexec sleep 30", pids.display());
        let asr = Asr::new(stand_in(dir.path(), &body, 0.0), |_| {});
        let stop = Arc::new(AtomicBool::new(false));
        let monitor = {
            let (pids, stop) = (pids.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut most = 0;
                while !stop.load(Ordering::Relaxed) {
                    most = most.max(running(&pids).0);
                    std::thread::sleep(Duration::from_millis(1));
                }
                most
            })
        };
        let mut random = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
            | 1;
        let mut next = move |n: u64| {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            random % n
        };
        let mut live = std::collections::VecDeque::new();
        for _ in 0..40 {
            // Starts while the last is still loading: it waits for its turn.
            live.push_back(asr.start());
            std::thread::sleep(Duration::from_millis(next(60)));
            match next(4) {
                0 => drop(live.pop_front()),
                1 => drop(live.pop_back()),
                2 => live.clear(),
                _ => {}
            }
            if live.len() > 3 {
                drop(live.pop_front());
            }
        }
        drop(live);
        wait_for("the last server to go", || running(&pids).0 == 0);
        stop.store(true, Ordering::Relaxed);
        let most = monitor.join().unwrap();
        let (_, started) = running(&pids);
        assert!(started > 1, "only {started} servers started");
        assert!(most <= 1, "{most} servers at once");
    }

    #[test]
    fn a_server_that_dies_while_recording_fails_the_dictation() {
        let _one = server::one_at_a_time();
        let dir = tempfile::tempdir().unwrap();
        let (host, die) = (dir.path().join("host"), dir.path().join("die"));
        // Tells the test its socket ("--host"), which the test then serves,
        // and dies when told to.
        let body = format!(
            "echo \"$6\" > '{}'\nwhile [ ! -e '{}' ]; do sleep 0.05; done\n\
             echo 'error: out of memory'\nexit 1",
            host.display(),
            die.display()
        );
        let asr = Asr::new(stand_in(dir.path(), &body, 0.0), |_| {});
        let mut d = asr.start();
        let mut socket = String::new();
        wait_for("the server to start", || {
            socket = std::fs::read_to_string(&host).unwrap_or_default();
            socket.ends_with('\n')
        });
        let asked = server::tests::answer_on(
            std::path::Path::new(socket.trim_end()),
            server::tests::answer("200 OK", r#"{"tokens":[1,2],"text":"hi"}"#),
        );
        // Loaded, and the prompt sent: recording, nothing left to do.
        let prime = std::iter::from_fn(|| asked.recv_timeout(Duration::from_secs(10)).ok())
            .find(|r| r.starts_with(b"POST /v1/audio/transcriptions"));
        assert!(prime.is_some(), "the prompt was sent");
        d.feed(&bytes(&audio(&[Quiet(500)])));
        assert_eq!(d.failure(), None);
        std::fs::write(&die, "").unwrap();
        let start = Instant::now();
        // While audio keeps coming, as it does while the user speaks.
        wait_for("the failure", || {
            d.feed(&bytes(&audio(&[Quiet(20)])));
            d.failure().is_some()
        });
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "noticed after {:?}",
            start.elapsed()
        );
        let e = d.failure().unwrap();
        assert!(
            e.contains("llama-server ended") && e.contains("out of memory"),
            "{e}"
        );
        assert_eq!(d.finish().unwrap_err(), e);
    }

    #[test]
    fn settings_read_the_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("asr.json");
        std::fs::write(
            &path,
            r#"{"server":"/s","model":"/m.gguf","mmproj":"/e.gguf","keywords":"~/k.txt","idleSeconds":5,"serverArgs":["-fa","on"]}"#,
        )
        .unwrap();
        let s = Settings::load(path.to_str().unwrap()).unwrap();
        assert_eq!(s.keywords.as_deref(), Some("~/k.txt"));
        assert_eq!(s.idle_seconds, 5.0);
        assert_eq!(s.server_args, ["-fa", "on"]);
        std::fs::write(
            &path,
            r#"{"server":"/s","model":"/m","mmproj":"/e","modle":1}"#,
        )
        .unwrap();
        assert!(
            Settings::load(path.to_str().unwrap()).is_err(),
            "typos are errors"
        );
        std::fs::write(&path, "{}").unwrap();
        let s = Settings::load(path.to_str().unwrap()).unwrap();
        assert_eq!(s.server, "llama-server");
        assert!(
            s.mmproj.ends_with("/mmproj-model-q4_k-convdw-f32.gguf"),
            "{}",
            s.mmproj
        );
        assert_eq!(s.idle_seconds, 0.0, "the model is freed at once");
        assert_eq!(s.cleanup, None, "no cleanup unless asked for");
        std::fs::write(&path, r#"{"cleanup":{"timeoutSeconds":2}}"#).unwrap();
        let c = Settings::load(path.to_str().unwrap())
            .unwrap()
            .cleanup
            .unwrap();
        assert_eq!((c.timeout_seconds, c.min_words), (2.0, 3));
        assert!(c.model.ends_with("/cleanup-1-q8_0.gguf"), "{}", c.model);
        std::fs::write(&path, r#"{"coreml":{"encoder":"~/e.mlpackage"}}"#).unwrap();
        let c = Settings::load(path.to_str().unwrap())
            .unwrap()
            .coreml
            .unwrap();
        assert_eq!((c.encoder.as_str(), c.frames.len()), ("~/e.mlpackage", 0));
    }

    /// A stand-in llama-server for both models: it writes its pid to
    /// `pids/<model>-<pid>` and its socket to `host-<model>`, and waits.
    /// The test serves the socket. Speech's model is "speech", the cleanup
    /// model "cleanup".
    struct Pair {
        dir: tempfile::TempDir,
        pids: PathBuf,
        logged: Arc<Mutex<Vec<String>>>,
    }

    impl Pair {
        /// `speech`: what Granite's stand-in does once it has said where it
        /// listens (`exec sleep 30` to wait to be served).
        fn new(speech: &str) -> Pair {
            let speech = if speech.is_empty() { ":" } else { speech };
            let dir = tempfile::tempdir().unwrap();
            let pids = dir.path().join("pids");
            std::fs::create_dir(&pids).unwrap();
            let body = format!(
                "cd '{d}'\nwhile [ $# -gt 0 ]; do case \"$1\" in --model) model=$2;; \
                 --host) host=$2;; esac; shift; done\n\
                 echo $$ > pids/$model-$$\necho \"$host\" > host-$model.new\n\
                 mv host-$model.new host-$model\n\
                 if [ \"$model\" = speech ]; then {speech}; fi\nexec sleep 30",
                d = dir.path().display()
            );
            server::tests::script(dir.path(), &body);
            Pair {
                dir,
                pids,
                logged: Arc::default(),
            }
        }

        fn settings(&self, timeout_seconds: f64) -> Settings {
            Settings {
                server: self
                    .dir
                    .path()
                    .join("fake-llama-server")
                    .to_string_lossy()
                    .into(),
                model: "speech".into(),
                mmproj: String::new(),
                coreml: None,
                keywords: None,
                idle_seconds: 0.0,
                server_args: Vec::new(),
                target_seconds: 5.0,
                max_seconds: 10.0,
                cleanup: Some(CleanupSettings {
                    model: "cleanup".into(),
                    server_args: Vec::new(),
                    timeout_seconds,
                    min_words: 3,
                }),
            }
        }

        fn asr(&self, settings: Settings) -> Asr {
            let logged = self.logged.clone();
            Asr::new(settings, move |m| logged.lock().unwrap().push(m))
        }

        /// Where the server of `model` listens, once it has started.
        fn socket(&self, model: &str) -> PathBuf {
            let path = self.dir.path().join(format!("host-{model}"));
            let mut text = String::new();
            wait_for(&format!("the {model} server"), || {
                text = std::fs::read_to_string(&path).unwrap_or_default();
                text.ends_with('\n')
            });
            PathBuf::from(text.trim_end())
        }

        fn started(&self, model: &str) -> bool {
            self.dir.path().join(format!("host-{model}")).exists()
        }

        /// How many servers of `model` run, and how many started.
        fn running(&self, model: &str) -> (usize, usize) {
            let pids: Vec<i32> = std::fs::read_dir(&self.pids)
                .unwrap()
                .filter_map(|e| {
                    let name = e.ok()?.file_name().into_string().ok()?;
                    name.strip_prefix(&format!("{model}-"))?.parse().ok()
                })
                .collect();
            let alive = pids.iter().filter(|&&p| server::tests::alive(p)).count();
            (alive, pids.len())
        }

        fn log(&self) -> String {
            self.logged.lock().unwrap().join("\n")
        }

        fn none_left(&self) {
            wait_for("every server to go", || {
                self.running("speech").0 == 0 && self.running("cleanup").0 == 0
            });
        }
    }

    fn reply(body: &str) -> String {
        server::tests::answer("200 OK", body)
    }

    /// Answers Granite's requests: `text` for a piece, after `before_piece`.
    fn speech_answers(
        text: &'static str,
        before_piece: impl Fn() + Send + Sync + 'static,
    ) -> impl Fn(&[u8]) -> String + Send + Sync + 'static {
        move |req| {
            let all = String::from_utf8_lossy(req);
            if all.starts_with("GET /health") {
                reply("{}")
            } else if all.starts_with("POST /tokenize") {
                reply(r#"{"tokens":[1,2]}"#)
            } else if all.contains("name=\"max_tokens\"\r\n\r\n1\r\n") {
                // The prompt, primed.
                reply(r#"{"text":""}"#)
            } else {
                before_piece();
                reply(&serde_json::json!({ "text": text }).to_string())
            }
        }
    }

    /// Answers the cleanup model's requests: `answer` (/completion's JSON)
    /// after `before`.
    fn cleanup_answers(
        answer: &'static str,
        before: impl Fn() + Send + Sync + 'static,
    ) -> impl Fn(&[u8]) -> String + Send + Sync + 'static {
        move |req| {
            let all = String::from_utf8_lossy(req);
            if all.starts_with("GET /health") {
                reply("{}")
            } else if all.starts_with("POST /tokenize") {
                reply(r#"{"tokens":[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15]}"#)
            } else {
                before();
                reply(answer)
            }
        }
    }

    const RAW: &str = "so we we should meet at three no at four tomorrow";
    const CLEAN: &str =
        r#"{"content":" So we should meet at 4 tomorrow.","stop_type":"eos","tokens_predicted":9}"#;

    /// A dictation of a second and a half with the stand-ins: Granite
    /// answers RAW, the cleanup model `answer` after `delay`. Returns what
    /// finish gave.
    fn clean_with(
        pair: &Pair,
        settings: Settings,
        answer: Option<&'static str>,
        delay: Duration,
    ) -> Result<String, String> {
        let asr = pair.asr(settings);
        let mut d = asr.start();
        server::tests::serve_on(&pair.socket("speech"), speech_answers(RAW, || {}));
        if let Some(answer) = answer {
            let (dir, pids) = (pair.dir.path().to_path_buf(), pair.pids.clone());
            std::thread::spawn(move || {
                let host = dir.join("host-cleanup");
                while !host.exists() && pids.exists() {
                    std::thread::sleep(Duration::from_millis(5));
                }
                let socket = std::fs::read_to_string(&host).unwrap_or_default();
                if socket.ends_with('\n') {
                    let socket = PathBuf::from(socket.trim_end());
                    server::tests::serve_on(
                        &socket,
                        cleanup_answers(answer, move || std::thread::sleep(delay)),
                    );
                }
            });
        }
        d.feed(&bytes(&audio(&[Speech(1_500)])));
        d.finish()
    }

    #[test]
    fn cleanup_loads_during_the_last_piece_and_its_text_is_typed() {
        let _one = server::one_at_a_time();
        let pair = Pair::new("");
        let asr = pair.asr(pair.settings(5.0));
        let mut d = asr.start();
        // The last piece takes until the cleanup server has started: it
        // started when the dictation ended, not after the piece.
        let overlapped = Arc::new(AtomicBool::new(false));
        let (o, dir) = (overlapped.clone(), pair.dir.path().to_path_buf());
        server::tests::serve_on(
            &pair.socket("speech"),
            speech_answers(RAW, move || {
                let start = Instant::now();
                while start.elapsed() < Duration::from_secs(5) {
                    if dir.join("host-cleanup").exists() {
                        o.store(true, Ordering::Relaxed);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }),
        );
        d.feed(&bytes(&audio(&[Speech(1_500)])));
        assert!(!pair.started("cleanup"), "nothing to clean yet");
        let finished = std::thread::spawn(move || d.finish());
        // Granite's server has stopped by the time the cleanup is asked
        // (killed: allow a moment for the kill to land, under load).
        let speech_left = Arc::new(Mutex::new(None));
        let (left, pids) = (speech_left.clone(), pair.pids.clone());
        let speech_running = move || {
            std::fs::read_dir(&pids)
                .unwrap()
                .filter_map(|e| {
                    let name = e.ok()?.file_name().into_string().ok()?;
                    name.strip_prefix("speech-")?.parse::<i32>().ok()
                })
                .filter(|&p| server::tests::alive(p))
                .count()
        };
        let asked = server::tests::serve_on(
            &pair.socket("cleanup"),
            cleanup_answers(CLEAN, move || {
                let mut n = speech_running();
                for _ in 0..20 {
                    if n == 0 {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                    n = speech_running();
                }
                *left.lock().unwrap() = Some(n);
            }),
        );
        let text = finished.join().unwrap().unwrap();
        assert_eq!(text, "So we should meet at 4 tomorrow.");
        assert!(
            overlapped.load(Ordering::Relaxed),
            "loaded while transcribing"
        );
        assert_eq!(
            *speech_left.lock().unwrap(),
            Some(0),
            "Granite was stopped first"
        );
        let completion = std::iter::from_fn(|| asked.try_recv().ok())
            .map(|r| String::from_utf8_lossy(&r).into_owned())
            .find(|r| r.starts_with("POST /completion"))
            .expect("asked for a completion");
        let body = &completion[completion.find("\r\n\r\n").unwrap() + 4..];
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(v["prompt"], format!("text: {RAW}\nout:"));
        assert_eq!(v["temperature"], 0);
        assert_eq!(v["n_predict"], 15 * 3 / 2 + 16);
        assert!(
            pair.log().contains(RAW),
            "the raw text is logged: {}",
            pair.log()
        );
        pair.none_left();
        assert_eq!(pair.running("cleanup").1, 1);
    }

    #[test]
    fn a_slow_cleanup_gives_the_raw_text_in_time() {
        let _one = server::one_at_a_time();
        let pair = Pair::new("");
        let start = Instant::now();
        let text = clean_with(
            &pair,
            pair.settings(0.5),
            Some(CLEAN),
            Duration::from_secs(3),
        );
        assert_eq!(text.unwrap(), RAW);
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "{:?}",
            start.elapsed()
        );
        assert!(
            pair.log().contains("no answer within 0.5 s"),
            "{}",
            pair.log()
        );
        pair.none_left();
    }

    #[test]
    fn a_cleanup_model_that_never_loads_gives_the_raw_text() {
        let _one = server::one_at_a_time();
        let pair = Pair::new("");
        let text = clean_with(&pair, pair.settings(0.3), None, Duration::ZERO);
        assert_eq!(text.unwrap(), RAW);
        assert!(pair.log().contains("still loading"), "{}", pair.log());
        assert_eq!(pair.running("cleanup").1, 1, "it was started");
        pair.none_left();
    }

    #[test]
    fn bad_answers_give_the_raw_text() {
        let _one = server::one_at_a_time();
        for (answer, why) in [
            (
                r#"{"content":" Sure! Here is the cleaned text: we meet at 4.","stop_type":"eos"}"#,
                "reply",
            ),
            (r#"{"content":"   ","stop_type":"eos"}"#, "empty"),
            (
                r#"{"content":" So we should meet at 4 tomorrow. So we","stop_type":"limit"}"#,
                "limit",
            ),
            (
                r#"{"content":" The weather tomorrow will be sunny and warm.","stop_type":"eos"}"#,
                "words",
            ),
            (r#"{"error":{"message":"out of cheese"}}"#, "out of cheese"),
        ] {
            let pair = Pair::new("");
            let text = clean_with(&pair, pair.settings(5.0), Some(answer), Duration::ZERO);
            assert_eq!(text.unwrap(), RAW, "{answer}");
            assert!(pair.log().contains(why), "{why}: {}", pair.log());
            pair.none_left();
        }
    }

    #[test]
    fn short_texts_are_not_cleaned() {
        let _one = server::one_at_a_time();
        let pair = Pair::new("");
        let asr = pair.asr(pair.settings(5.0));
        let mut d = asr.start();
        server::tests::serve_on(&pair.socket("speech"), speech_answers("Yes.", || {}));
        d.feed(&bytes(&audio(&[Speech(500)])));
        assert_eq!(d.finish().unwrap(), "Yes.");
        assert!(pair.log().contains("skipped, 1 words"), "{}", pair.log());
        pair.none_left();
    }

    #[test]
    fn a_failed_transcription_is_not_cleaned_and_stops_both() {
        let _one = server::one_at_a_time();
        let pair = Pair::new("sleep 0.3; echo 'error: no such model'; exit 1");
        let asr = pair.asr(pair.settings(5.0));
        let mut d = asr.start();
        d.feed(&bytes(&audio(&[Speech(1_000)])));
        let e = d.finish().unwrap_err();
        assert!(e.contains("no such model"), "{e}");
        pair.none_left();
    }

    #[test]
    fn there_is_never_more_than_one_server_of_each_kind() {
        let _one = server::one_at_a_time();
        // Granite fails after a while; the cleanup model never loads.
        let pair = Arc::new(Pair::new("sleep 0.2; exit 1"));
        let asr = Arc::new(pair.asr(pair.settings(5.0)));
        let stop = Arc::new(AtomicBool::new(false));
        let monitor = {
            let (pair, stop) = (pair.clone(), stop.clone());
            std::thread::spawn(move || {
                let (mut speech, mut cleanup) = (0, 0);
                while !stop.load(Ordering::Relaxed) {
                    speech = pair.running("speech").0.max(speech);
                    cleanup = pair.running("cleanup").0.max(cleanup);
                    std::thread::sleep(Duration::from_millis(1));
                }
                (speech, cleanup)
            })
        };
        let finishing: Vec<_> = (0..12)
            .map(|i| {
                let mut d = asr.start();
                d.feed(&bytes(&audio(&[Speech(300)])));
                std::thread::sleep(Duration::from_millis(30 * (i % 4)));
                std::thread::spawn(move || d.finish())
            })
            .collect();
        for f in finishing {
            assert!(f.join().unwrap().is_err());
        }
        pair.none_left();
        stop.store(true, Ordering::Relaxed);
        let (speech, cleanup) = monitor.join().unwrap();
        assert!(pair.running("cleanup").1 > 1, "cleanup servers started");
        assert!(speech <= 1, "{speech} speech servers at once");
        assert!(cleanup <= 1, "{cleanup} cleanup servers at once");
    }

    /// The samples of a 16-bit mono WAV file.
    fn read_wav(path: &str) -> Vec<u8> {
        let bytes = std::fs::read(path).unwrap();
        let mut at = 12;
        while at + 8 <= bytes.len() {
            let size = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
            if &bytes[at..at + 4] == b"data" {
                return bytes[at + 8..(at + 8 + size).min(bytes.len())].to_vec();
            }
            at += 8 + size + size % 2;
        }
        panic!("{path}: no data");
    }

    /// With the real server and model, on demand:
    ///
    /// ```sh
    /// WISPR_ASR_SERVER=.../llama-server WISPR_ASR_MODEL=.../granite.gguf \
    /// WISPR_ASR_MMPROJ=.../mmproj.gguf WISPR_ASR_WAVS=a.wav:b.wav \
    /// [WISPR_ASR_KEYWORDS=terms.txt] [WISPR_ASR_SPEED=1] \
    ///   cargo test real_model -- --ignored --nocapture
    /// ```
    ///
    /// Plays the recordings (16 kHz mono) one after the other with 0.8 s of
    /// silence between, at SPEED times real time, and prints the text.
    #[test]
    #[ignore]
    fn real_model_transcribes_recordings() {
        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} is not set"));
        let settings = Settings {
            server: env("WISPR_ASR_SERVER"),
            model: env("WISPR_ASR_MODEL"),
            mmproj: env("WISPR_ASR_MMPROJ"),
            coreml: None,
            keywords: std::env::var("WISPR_ASR_KEYWORDS")
                .ok()
                .filter(|k| !k.is_empty()),
            idle_seconds: 0.0,
            server_args: Vec::new(),
            target_seconds: 5.0,
            max_seconds: 10.0,
            cleanup: None,
        };
        let speed: f64 = std::env::var("WISPR_ASR_SPEED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1.0);
        let mut pcm = Vec::new();
        for (i, path) in env("WISPR_ASR_WAVS").split(':').enumerate() {
            if i > 0 {
                pcm.extend(std::iter::repeat_n(0u8, 2 * 12_800));
            }
            pcm.extend(read_wav(path));
        }
        let asr = Asr::new(settings, |m| eprintln!("log: {m}"));
        let start = Instant::now();
        let mut d = asr.start();
        for (i, chunk) in pcm.chunks(1_280).enumerate() {
            d.feed(chunk);
            let due = Duration::from_secs_f64((i + 1) as f64 * 0.04 / speed);
            if let Some(wait) = due.checked_sub(start.elapsed()) {
                std::thread::sleep(wait);
            }
        }
        let end = Instant::now();
        let text = d.finish().unwrap();
        eprintln!(
            "{:.1} s of audio, text {:.2} s after the end:\n{text}",
            pcm.len() as f64 / 32_000.0,
            end.elapsed().as_secs_f64()
        );
        assert!(!text.is_empty());
    }

    /// Accuracy and time against piece length, with the real server and
    /// model, on demand:
    ///
    /// ```sh
    /// WISPR_ASR_SERVER=... WISPR_ASR_MODEL=... WISPR_ASR_MMPROJ=... \
    /// WISPR_ASR_RECORDINGS=list.tsv WISPR_ASR_LIMITS=5:10,10:20,30:30 \
    /// WISPR_ASR_OUT=pieces.jsonl [WISPR_ASR_KEYWORDS=terms.txt] \
    ///   cargo test --release real_model_piece -- --ignored --nocapture
    /// ```
    ///
    /// Each line of the list is a name, a tab and WAV files separated by
    /// colons, played one after the other with 0.3 s of room noise between.
    /// Each recording is cut with each target:longest pair and transcribed
    /// piece by piece; one JSON line per recording and pair has the text and
    /// each piece's length and time.
    #[test]
    #[ignore]
    fn real_model_piece_lengths() {
        use std::io::Write;
        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} is not set"));
        let settings = Settings {
            server: env("WISPR_ASR_SERVER"),
            model: env("WISPR_ASR_MODEL"),
            mmproj: env("WISPR_ASR_MMPROJ"),
            coreml: None,
            keywords: None,
            idle_seconds: 0.0,
            server_args: Vec::new(),
            target_seconds: 30.0,
            max_seconds: 30.0,
            cleanup: None,
        };
        let keywords = std::env::var("WISPR_ASR_KEYWORDS")
            .ok()
            .filter(|k| !k.is_empty())
            .map(|k| prompt::parse_keywords(&std::fs::read_to_string(k).unwrap()))
            .unwrap_or_default();
        let ask = prompt::prompt(&keywords);
        let longest = 30 * RATE;
        let mut server =
            Server::spawn(&settings, prompt::context_for(&ask, longest), &|_| {}).unwrap();
        server.ready(&|| false).unwrap();
        let primed = Instant::now();
        server.transcribe(&[0; RATE / 10], &ask, 1).unwrap();
        eprintln!(
            "{} keywords, {} tokens, primed in {:.2} s",
            keywords.len(),
            server.tokens(&ask).unwrap(),
            primed.elapsed().as_secs_f64()
        );
        let noise: Vec<i16> = (0..RATE * 3 / 10)
            .map(|i| ((i * 7_919 % 61) as i16 - 30) / 2)
            .collect();
        let mut out = std::fs::File::create(env("WISPR_ASR_OUT")).unwrap();
        let list = std::fs::read_to_string(env("WISPR_ASR_RECORDINGS")).unwrap();
        for line in list.lines().filter(|l| !l.is_empty()) {
            let (name, wavs) = line.split_once('\t').unwrap();
            let mut pcm = Vec::new();
            for (i, path) in wavs.split(':').enumerate() {
                if i > 0 {
                    pcm.extend_from_slice(&noise);
                }
                pcm.extend(
                    read_wav(path)
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|b| i16::from_le_bytes(*b)),
                );
            }
            for pair in env("WISPR_ASR_LIMITS").split(',') {
                let (target, max) = pair.split_once(':').unwrap();
                let limits = Limits::new(target.parse().unwrap(), max.parse().unwrap());
                let mut segmenter = Segmenter::new(limits);
                segmenter.push(&pcm);
                segmenter.finish(pcm.len());
                let (mut done, mut texts, mut pieces) = (0, Vec::new(), Vec::new());
                while let Some(job) = next_job(segmenter.cuts(), done, limits.merge_samples()) {
                    if segmenter.has_speech(job.clone()) {
                        let piece = &pcm[job.clone()];
                        let asked = Instant::now();
                        let text = server
                            .transcribe(piece, &ask, prompt::max_tokens(piece.len()))
                            .unwrap();
                        pieces.push((
                            piece.len() as f64 / RATE as f64,
                            asked.elapsed().as_secs_f64(),
                        ));
                        texts.push(text);
                    }
                    done = job.end;
                }
                let record = serde_json::json!({
                    "recording": name,
                    "limits": pair,
                    "keywords": keywords.len(),
                    "audio": pcm.len() as f64 / RATE as f64,
                    "pieces": pieces,
                    "text": texts.join(" "),
                });
                writeln!(out, "{record}").unwrap();
                eprintln!("{name} {pair}: {} pieces", pieces.len());
            }
        }
    }
}
