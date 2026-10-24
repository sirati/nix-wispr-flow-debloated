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
//! Use: `Asr::start` when a dictation starts, `Dictation::feed` with its
//! audio as it arrives, `Dictation::finish` for the text when it ends.
//! Dropping an unfinished Dictation cancels it.

mod prompt;
mod segment;
mod server;

use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;

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
    /// Terms the text should spell as listed, one per line (prompt.rs).
    /// Read when a dictation starts, so edits apply to the next one.
    #[serde(default)]
    pub keywords: Option<String>,
    /// How long the model stays loaded after a dictation, for the next.
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

/// Transcribes the pieces of a dictation: llama-server, or a stand-in in
/// tests.
trait Engine: Send + 'static {
    /// Processes the prompt while no piece is ready yet. The server keeps it
    /// cached for the pieces, so a long keyword list is read once, not once
    /// a piece.
    fn prime(&mut self) -> Result<(), String>;
    fn transcribe(&mut self, pcm: &[i16]) -> Result<String, String>;
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
        if !lease.server.as_ref().is_some_and(|s| s.context >= context) {
            lease.server = None;
            lease.server = Some(Server::spawn(settings, context)?);
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
    log: Log,
}

impl Asr {
    pub fn new(settings: Settings, log: impl Fn(String) + Send + Sync + 'static) -> Asr {
        Asr {
            settings: Arc::new(settings),
            slot: Arc::default(),
            log: Arc::new(log),
        }
    }

    /// A dictation starts: the model starts loading now.
    pub fn start(&self) -> Dictation {
        let (settings, slot, log) = (self.settings.clone(), self.slot.clone(), self.log.clone());
        Dictation::spawn(
            move |cancelled: &dyn Fn() -> bool| Llama::load(&settings, &slot, &log, cancelled),
            self.settings.limits(),
            self.log.clone(),
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
    /// The odd byte of the last feed.
    carry: Option<u8>,
    finished: bool,
}

impl Dictation {
    /// Runs `work` on its own thread. The engine is dropped once the text is
    /// out, which frees the model (or hands it on, see Slot).
    fn spawn<E: Engine>(
        load: impl FnOnce(&dyn Fn() -> bool) -> Result<E, String> + Send + 'static,
        limits: Limits,
        log: Log,
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
            // The text first, then the memory.
            let _ = tx.send(text);
            drop(engine);
        });
        Dictation {
            shared,
            result,
            carry: None,
            finished: false,
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
    /// the whole text.
    pub fn finish(mut self) -> Result<String, String> {
        self.finished = true;
        {
            let mut state = self.shared.state();
            state.ended = Some(Instant::now());
            let total = state.audio.len();
            state.segmenter.finish(total);
        }
        self.shared.wake.notify_all();
        self.result
            .recv()
            .unwrap_or_else(|_| Err("the transcription stopped".into()))
    }
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
                state = shared.wake.wait(state).expect("dictation lock poisoned");
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

    /// Settings whose server is a shell script that only waits.
    fn sleeper(dir: &std::path::Path, idle_seconds: f64) -> Settings {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-llama-server");
        std::fs::write(&path, "#!/bin/sh\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        Settings {
            server: path.to_string_lossy().into_owned(),
            model: String::new(),
            mmproj: String::new(),
            keywords: None,
            idle_seconds,
            server_args: Vec::new(),
            target_seconds: 5.0,
            max_seconds: 10.0,
        }
    }

    /// A turn with a fresh server, as after a successful load.
    fn loaded_lease(slot: &Arc<Slot>, settings: &Settings) -> (Lease, PathBuf) {
        let mut lease = slot.take(settings, &|| false).unwrap();
        let server = Server::spawn(settings, 1_024).unwrap();
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
        let dir = tempfile::tempdir().unwrap();
        let settings = sleeper(dir.path(), 10.0);
        let slot = Arc::new(Slot::default());
        let (mut lease, _) = loaded_lease(&slot, &settings);
        lease.healthy = false;
        drop(lease);
        assert!(!kept(&slot));
        assert!(slot.take(&settings, &|| false).unwrap().server.is_none());
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
            keywords: std::env::var("WISPR_ASR_KEYWORDS")
                .ok()
                .filter(|k| !k.is_empty()),
            idle_seconds: 0.0,
            server_args: Vec::new(),
            target_seconds: 5.0,
            max_seconds: 10.0,
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
            keywords: None,
            idle_seconds: 0.0,
            server_args: Vec::new(),
            target_seconds: 30.0,
            max_seconds: 30.0,
        };
        let keywords = std::env::var("WISPR_ASR_KEYWORDS")
            .ok()
            .filter(|k| !k.is_empty())
            .map(|k| prompt::parse_keywords(&std::fs::read_to_string(k).unwrap()))
            .unwrap_or_default();
        let ask = prompt::prompt(&keywords);
        let longest = 30 * RATE;
        let mut server = Server::spawn(&settings, prompt::context_for(&ask, longest)).unwrap();
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
