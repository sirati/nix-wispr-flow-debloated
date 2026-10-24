// Safe Rust, except the macOS modules that call C APIs: mic_mac.rs (objc2
// marks AVFoundation calls unsafe until they are reviewed upstream),
// hotkey/mac.rs and typing/mac.rs; each allows it (see their headers).
#![deny(unsafe_code)]
//! Stands in for Wispr Flow's hidden windows while they are not rendered:
//! records the microphone for its dictation and draws the speech indicator.
//! Spawned by the Electron shim (wispr-headless/shim.js), which speaks to it
//! in JSON lines on stdin/stdout. Exits when stdin closes.
//!
//! It can also watch the dictation shortcut itself (`--hotkey`, hotkey/)
//! and type text into the focused app (typing/), for dictation that does
//! not go through Wispr. `--hotkey-test` and `--type-test` try them out.

mod asr;
mod bar;
mod hotkey;
#[cfg(target_os = "macos")]
mod mic_mac;
mod mics;
#[cfg(target_os = "macos")]
mod pill_mac;
#[cfg(target_os = "linux")]
mod pill_x11;
mod protocol;
mod recorder;
mod resample;
#[cfg(target_os = "linux")]
mod text;
mod typing;
mod wave;

#[cfg(target_os = "macos")]
use pill_mac as pill;
#[cfg(target_os = "linux")]
use pill_x11 as pill;

use std::io::{BufRead, Read, Write};
use std::process::{Command as Process, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bar::{BarEvent, PillCommand};
use hotkey::Hotkey;
use protocol::{Backend, Command, Event};
use recorder::{Progress, Recording};

/// What the process is run for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Run {
    /// Wispr's recorder and bar, for the shim.
    Shim,
    /// Prints the keys and what the `--hotkey` bindings make of them.
    HotkeyTest,
    /// Types stdin into the focused app.
    TypeTest,
}

struct Config {
    run: Run,
    /// `--hotkey` bindings (hotkey/keys.rs).
    hotkeys: Vec<String>,
    typing: typing::Options,
    /// The capture command without its device (see `capture_for`).
    capture: Vec<String>,
    /// The input given on the command line (Nix's micTarget), which wins over
    /// the one Wispr would pick.
    target: Option<String>,
    device: String,
    player: Vec<String>,
    sounds: Option<String>,
    /// Figtree for the bar's text (Linux; macOS uses its system font).
    fonts: Option<String>,
    /// Where dictations this process starts itself go (`dictate`), unless
    /// one says otherwise. Default: the local model if there is one.
    backend: Option<Backend>,
    /// The local model's settings (asr/mod.rs).
    asr_config: Option<String>,
    /// The bar's fill for Wispr and for the local model.
    fill: [u8; 3],
    local_fill: [u8; 3],
}

fn config() -> Config {
    let mut capture = vec![
        "pw-cat".into(),
        "--record".into(),
        "--raw".into(),
        "--rate".into(),
        "16000".into(),
        "--channels".into(),
        "1".into(),
        "--format".into(),
        "s16".into(),
        "--latency".into(),
        "20ms".into(),
        "--media-category".into(),
        "Capture".into(),
        "--media-role".into(),
        "Communication".into(),
    ];
    let mut device = "Default (PipeWire)".to_string();
    let mut player = vec!["pw-cat".into(), "--playback".into()];
    let mut sounds = None;
    let mut fonts = None;
    let mut target = None;
    let mut backend = None;
    let mut asr_config = None;
    let mut fill = bar::WISPR_FILL;
    let mut local_fill = bar::LOCAL_FILL;
    let mut run = Run::Shim;
    let mut hotkeys = Vec::new();
    let mut typing = typing::Options::default();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            // A dictation shortcut, e.g. RightAlt or toggle:RightAlt+Space;
            // may be given more than once.
            "--hotkey" => hotkeys.extend(args.next()),
            // auto, ime, keys or paste (typing/mod.rs).
            "--type-with" => {
                if let Some(m) = args.next() {
                    match typing::Method::parse(&m) {
                        Ok(m) => typing.method = m,
                        Err(e) => eprintln!("wispr-headless: --type-with {e}"),
                    }
                }
            }
            "--wl-copy" => typing.wl_copy = args.next(),
            "--hotkey-test" => run = Run::HotkeyTest,
            "--type-test" => run = Run::TypeTest,
            "--pw-cat" => {
                if let Some(p) = args.next() {
                    capture[0] = p.clone();
                    player[0] = p;
                }
            }
            "--target" => target = args.next(),
            "--sounds" => sounds = args.next(),
            "--font-dir" => fonts = args.next(),
            // "wispr" or "local".
            "--backend" => backend = args.next().as_deref().and_then(Backend::parse),
            "--asr-config" => asr_config = args.next().filter(|c| !c.is_empty()),
            // "#rrggbb".
            "--bar-color" => fill = args.next().and_then(|c| color(&c)).unwrap_or(fill),
            "--local-bar-color" => {
                local_fill = args.next().and_then(|c| color(&c)).unwrap_or(local_fill)
            }
            // Any command that writes 16 kHz s16 mono PCM to stdout: ffmpeg on
            // macOS, a stand-in microphone in tests.
            "--capture-cmd" => {
                if let Some(c) = args.next() {
                    capture = c.split_whitespace().map(String::from).collect();
                    device = "test source".into();
                }
            }
            // What Wispr is told the device is called (after --capture-cmd).
            "--device-name" => {
                if let Some(d) = args.next() {
                    device = d;
                }
            }
            // A command that plays the WAV file given as its last argument.
            "--player-cmd" => {
                if let Some(c) = args.next() {
                    player = c.split_whitespace().map(String::from).collect();
                }
            }
            "--version" => {
                println!("wispr-headless {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            _ => {}
        }
    }
    Config {
        run,
        hotkeys,
        typing,
        capture,
        target: target.filter(|t| !t.is_empty()),
        device,
        player,
        sounds,
        fonts,
        backend,
        asr_config,
        fill,
        local_fill,
    }
}

/// "#rrggbb" (or without "#") as red, green, blue.
fn color(hex: &str) -> Option<[u8; 3]> {
    let hex = hex.strip_prefix('#').unwrap_or(hex);
    if hex.len() != 6 || !hex.is_ascii() {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some([byte(0)?, byte(2)?, byte(4)?])
}

/// The capture command for one recording, and the device name Wispr is told:
/// the input from the command line if given, else the one Wispr's ranking
/// picks (`chosen`), else the system default.
fn capture_for(cfg: &Config, chosen: Option<&mics::Native>) -> (Vec<String>, String) {
    let mut capture = cfg.capture.clone();
    let mut device = cfg.device.clone();
    let input = cfg
        .target
        .clone()
        .map(|t| (t.clone(), t))
        .or_else(|| chosen.map(|n| (n.id.clone(), n.name.clone())));
    match mics::Backend::of(&capture) {
        mics::Backend::PipeWire => {
            if let Some((id, name)) = input {
                capture.extend(["--target".into(), id]);
                device = name;
            }
            capture.push("-".into());
        }
        mics::Backend::AvFoundation => {
            if let Some((id, name)) = input {
                for arg in capture.iter_mut().filter(|a| *a == ":default") {
                    *arg = format!(":{id}");
                }
                device = name;
            }
        }
        mics::Backend::Fixed => {}
    }
    (capture, device)
}

/// Starts a recording of `chosen` (see `capture_for`). On macOS the daemon
/// captures the microphone itself, which starts sooner; the capture command
/// (ffmpeg) is the fallback.
fn start_recording(
    cfg: &Config,
    uuid: &str,
    chosen: Option<&mics::Native>,
    clock_ms: f64,
    out: &Out,
    ready: &mut Ready,
) -> Result<Recording, String> {
    let (capture, device) = capture_for(cfg, chosen);
    #[cfg(target_os = "macos")]
    if let Some(unit) = ready.prepare(cfg, chosen) {
        match Recording::start_native(uuid.to_string(), unit, device.clone(), clock_ms) {
            Ok(r) => return Ok(r),
            Err(_) => {
                // Perhaps the device went away: prepare afresh next time.
                ready.unit = None;
                out.send(&Event::Log {
                    msg: "native capture failed; using the capture command".into(),
                });
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (out, ready);
    Recording::start(uuid.to_string(), &capture, device, clock_ms)
}

/// A recording started ahead of Wispr's request (`early` in serve()).
fn start_early(
    cfg: &Config,
    chosen: Option<&mics::Native>,
    clock_ms: f64,
    out: &Out,
    ready: &mut Ready,
) -> Option<(Recording, Instant)> {
    start_recording(cfg, "", chosen, clock_ms, out, ready)
        .ok()
        .map(|r| (r, Instant::now()))
}

/// macOS: the input unit kept prepared (microphone off) for the input the
/// next recording will use, so starting it is all a dictation waits for.
#[derive(Default)]
struct Ready {
    #[cfg(target_os = "macos")]
    unit: Option<Arc<mic_mac::Prepared>>,
}

impl Ready {
    /// The unit for the input `capture_for` would pick, prepared if it is
    /// not yet (macOS with the AVFoundation capture command only).
    #[cfg(target_os = "macos")]
    fn prepare(
        &mut self,
        cfg: &Config,
        chosen: Option<&mics::Native>,
    ) -> Option<Arc<mic_mac::Prepared>> {
        if mics::Backend::of(&cfg.capture) != mics::Backend::AvFoundation {
            return None;
        }
        let input = cfg.target.as_deref().or(chosen.map(|n| n.id.as_str()));
        if !self
            .unit
            .as_ref()
            .is_some_and(|u| u.input.as_deref() == input)
        {
            self.unit = mic_mac::Prepared::new(input).ok().map(Arc::new);
        }
        self.unit.clone()
    }

    #[cfg(not(target_os = "macos"))]
    fn prepare(&mut self, _cfg: &Config, _chosen: Option<&mics::Native>) -> Option<()> {
        None
    }
}

/// How long an early start waits for Wispr's request to record.
const EARLY_START_PATIENCE: Duration = Duration::from_millis(500);

struct Out(Arc<Mutex<std::io::Stdout>>);

impl Out {
    fn send(&self, event: &Event) {
        let mut out = self.0.lock().expect("stdout lock poisoned");
        let _ = writeln!(out, "{}", protocol::encode(event));
        let _ = out.flush();
    }
}

fn play(cfg: &Config, name: &str, out: &Out) {
    let Some(dir) = &cfg.sounds else { return };
    let file = match name {
        "start" => "dictation-start.wav",
        "stop" => "dictation-stop.wav",
        "paste" => "paste.wav",
        "achievement" => "achievement.wav",
        "popo" => "popo-lock.wav",
        _ => return,
    };
    let path = format!("{dir}/{file}");
    let (program, args) = match cfg.player.split_first() {
        Some(p) => p,
        None => return,
    };
    if let Err(e) = Process::new(program)
        .args(args)
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        out.send(&Event::Log {
            msg: format!("cannot play {path}: {e}"),
        });
    }
}

/// A dictation transcribed here: its recording and its transcription.
struct Local {
    recording: Recording,
    dictation: asr::Dictation,
}

/// Ends a local dictation: the rest of its audio goes to the model, and the
/// text comes back as Command::Transcribed.
fn end_local(
    local: Local,
    commands: &mpsc::Sender<Option<Command>>,
    pill_tx: &mpsc::Sender<PillCommand>,
) {
    let Local {
        recording,
        mut dictation,
    } = local;
    dictation.feed(&recording.finish_pcm());
    let _ = pill_tx.send(PillCommand::Status("processing".into()));
    let commands = commands.clone();
    std::thread::spawn(move || {
        let result = dictation.finish();
        let _ = commands.send(Some(Command::Transcribed { result }));
    });
}

/// A thread that types the transcripts sent to it, in order (a paste on
/// macOS waits half a second to put the clipboard back).
fn typing_thread(cfg: &Config, out: &Out) -> Option<mpsc::Sender<String>> {
    let log_out = Out(out.0.clone());
    let log: typing::Log = Arc::new(move |msg| log_out.send(&Event::Log { msg }));
    let mut typist = match typing::Typist::new(cfg.typing.clone(), log.clone()) {
        Ok(t) => t,
        Err(e) => {
            log(format!("transcripts will not be typed: {e}"));
            return None;
        }
    };
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for text in rx {
            if let Err(e) = typist.type_text(&text) {
                log(format!("typing the transcript: {e}"));
            }
        }
    });
    Some(tx)
}

/// A local dictation that produced no text.
fn local_failed(out: &Out, pill_tx: &mpsc::Sender<PillCommand>, error: String) {
    let _ = pill_tx.send(PillCommand::Status("error".into()));
    out.send(&Event::TranscriptFailed { error });
}

/// `--hotkey-test`: prints each key going down and up, by the name a
/// binding would use, and what the `--hotkey` bindings make of it. Without
/// bindings every device with keys is read, to find a key's name.
fn hotkey_test(cfg: &Config) -> ! {
    let bindings = hotkey::parse_bindings(&cfg.hotkeys).unwrap_or_else(|e| {
        eprintln!("wispr-headless: --hotkey {e}");
        std::process::exit(2)
    });
    let wanted: Vec<hotkey::Key> = if bindings.is_empty() {
        (1..0x300).collect()
    } else {
        bindings.iter().flat_map(|b| b.keys.clone()).collect()
    };
    let (tx, rx) = mpsc::channel();
    let log: hotkey::Log = Arc::new(|msg| eprintln!("{msg}"));
    if let Err(e) = hotkey::watch(&wanted, tx, log) {
        eprintln!("wispr-headless: {e}");
        std::process::exit(1);
    }
    let out = std::cell::RefCell::new(std::io::stdout().lock());
    hotkey::drive(
        hotkey::Chord::new(bindings, hotkey::ARM),
        rx,
        |key, down| {
            let mut out = out.borrow_mut();
            let state = if down { "down" } else { "up" };
            let _ = writeln!(out, "{} {state}", hotkey::key_name(key));
            let _ = out.flush();
        },
        |e, binding| {
            let mut out = out.borrow_mut();
            let _ = match &binding.backend {
                Some(backend) => writeln!(out, "  {} {backend}", e.name()),
                None => writeln!(out, "  {}", e.name()),
            };
            let _ = out.flush();
        },
    );
    std::process::exit(1)
}

/// `--type-test`: types stdin (one final new line dropped) into whatever
/// has focus three seconds later (tests: WISPR_HEADLESS_TEST_TYPE_DELAY_MS).
fn type_test(cfg: &Config) -> ! {
    let mut text = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut text) {
        eprintln!("wispr-headless: stdin: {e}");
        std::process::exit(2);
    }
    let text = text.strip_suffix('\n').unwrap_or(&text);
    let log: typing::Log = Arc::new(|msg| eprintln!("{msg}"));
    let mut typist = typing::Typist::new(cfg.typing.clone(), log).unwrap_or_else(|e| {
        eprintln!("wispr-headless: {e}");
        std::process::exit(1)
    });
    let delay = std::env::var("WISPR_HEADLESS_TEST_TYPE_DELAY_MS")
        .ok()
        .and_then(|ms| ms.parse().ok())
        .unwrap_or(3000);
    eprintln!("typing in {} seconds; focus a text field", delay / 1000);
    std::thread::sleep(Duration::from_millis(delay));
    match typist.type_text(text) {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("wispr-headless: {e}");
            std::process::exit(1)
        }
    }
}

fn main() {
    let cfg = config();
    match cfg.run {
        Run::HotkeyTest => hotkey_test(&cfg),
        Run::TypeTest => type_test(&cfg),
        Run::Shim => {}
    }
    let out = Out(Arc::new(Mutex::new(std::io::stdout())));

    // The flow bar keeps the main thread (AppKit needs it on macOS); commands
    // and the recorder run on their own, and the process ends with them.
    let (pill_tx, pill_rx) = mpsc::channel::<PillCommand>();
    let _ = pill_tx.send(PillCommand::Fills {
        wispr: cfg.fill,
        local: cfg.local_fill,
    });
    let fonts = cfg.fonts.clone();
    {
        let out = Out(out.0.clone());
        std::thread::spawn(move || {
            serve(&cfg, &out, &pill_tx);
            std::process::exit(0);
        });
    }
    let log_out = Out(out.0.clone());
    let event_out = Out(out.0.clone());
    pill::run(
        pill_rx,
        fonts.as_deref(),
        move |msg| log_out.send(&Event::Log { msg }),
        // Clicks and notification results go straight to the shim.
        move |e| {
            event_out.send(&match e {
                BarEvent::Click(action) => Event::Click {
                    action: action.to_string(),
                },
                BarEvent::NoticeAction(index) => Event::NoticeAction { index },
                BarEvent::NoticeClosed(reason) => Event::NoticeClosed {
                    reason: reason.to_string(),
                },
            })
        },
    );
    // The bar only stops once serve() is done and about to exit.
    loop {
        std::thread::park();
    }
}

fn serve(cfg: &Config, out: &Out, pill_tx: &mpsc::Sender<PillCommand>) {
    let clock = Instant::now();
    let clock_ms = || clock.elapsed().as_secs_f64() * 1_000.0;

    // The system's inputs, listed in the background (ffmpeg takes a moment)
    // at start, when Wispr's ranking changes and after each recording, so a
    // start never waits for it.
    let backend = mics::Backend::of(&cfg.capture);
    // Commands arrive on their own thread so audio can be polled in between.
    let (cmd_tx, cmd_rx) = mpsc::channel::<Option<Command>>();
    let inputs = Arc::new(Mutex::new(Vec::<mics::Native>::new()));
    let (relist, relist_rx) = mpsc::channel::<()>();
    if cfg.target.is_none() && backend != mics::Backend::Fixed {
        let inputs = inputs.clone();
        let program = cfg.capture.first().cloned().unwrap_or_default();
        let done = cmd_tx.clone();
        std::thread::spawn(move || {
            while relist_rx.recv().is_ok() {
                while relist_rx.try_recv().is_ok() {}
                let found = mics::list(backend, &program);
                *inputs.lock().expect("inputs lock poisoned") = found;
                // So the next recording's input gets prepared.
                if done.send(Some(Command::Relisted)).is_err() {
                    return;
                }
            }
        });
    }
    let _ = relist.send(());
    let mut ranked: Vec<mics::Ranked> = Vec::new();
    let mut ready = Ready::default();
    // Prepares for the input the next recording will use (macOS).
    let prepare = |ready: &mut Ready, ranked: &[mics::Ranked]| {
        let inputs = inputs.lock().expect("inputs lock poisoned");
        let _ = ready.prepare(cfg, mics::choose(ranked, &inputs));
    };
    prepare(&mut ready, &ranked);

    // Local transcription, if configured; nothing is loaded until it is used.
    let asr = cfg
        .asr_config
        .as_deref()
        .and_then(|path| match asr::Settings::load(path) {
            Ok(settings) => {
                let log_out = Out(out.0.clone());
                Some(asr::Asr::new(settings, move |msg| {
                    log_out.send(&Event::Log { msg })
                }))
            }
            Err(e) => {
                out.send(&Event::Log {
                    msg: format!("no local transcription: {e}"),
                });
                None
            }
        });
    let backend = cfg.backend.unwrap_or(if asr.is_some() {
        Backend::Local
    } else {
        Backend::Wispr
    });
    let internal = cmd_tx.clone();

    // Our own dictation shortcut (--hotkey), as dictate commands. A hold
    // starts recording as it goes down, for whichever backend it turns out
    // to be for; Wispr adopts that recording as it does Early's.
    if !cfg.hotkeys.is_empty() {
        let log_out = Out(out.0.clone());
        let log: hotkey::Log = Arc::new(move |msg| log_out.send(&Event::Log { msg }));
        let to_serve = cmd_tx.clone();
        let started = hotkey::parse_bindings(&cfg.hotkeys).and_then(|bindings| {
            for b in &bindings {
                if let Some(name) = &b.backend {
                    Backend::parse(name).ok_or(format!("{name}: no such backend"))?;
                }
            }
            hotkey::spawn(
                bindings,
                move |h, binding| {
                    let command = match h {
                        Hotkey::Down => Command::Early,
                        Hotkey::Pressed => Command::Dictate {
                            backend: binding.backend.as_deref().and_then(Backend::parse),
                        },
                        Hotkey::HandsFree => return,
                        Hotkey::Released => Command::DictateEnd,
                        Hotkey::Cancelled => Command::DictateCancel,
                    };
                    let _ = to_serve.send(Some(command));
                },
                log.clone(),
            )
        });
        if let Err(e) = started {
            log(format!("no dictation shortcut: {e}"));
        }
    }

    // Types what the local model heard; set up now, as a uinput keyboard
    // needs a moment before the compositor takes keys from it.
    let typist = asr.as_ref().and_then(|_| typing_thread(cfg, out));

    {
        let log_out = Out(out.0.clone());
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                match protocol::parse(&line) {
                    Ok(c) => {
                        if cmd_tx.send(Some(c)).is_err() {
                            return;
                        }
                    }
                    Err(e) => log_out.send(&Event::Log {
                        msg: format!("bad command: {e}"),
                    }),
                }
            }
            let _ = cmd_tx.send(None);
        });
    }

    out.send(&Event::Ready);
    let mut recording: Option<Recording> = None;
    // Started early, when Wispr's dictation begins ("initializing"), before
    // it asks to record (about 25 ms later): handed over with everything it
    // heard, or dropped if no request comes.
    let mut early: Option<(Recording, Instant)> = None;
    // A dictation of our own (`dictate`) that the local model transcribes.
    let mut local: Option<Local> = None;
    // A dictation of our own that Wispr runs: its bar's start was clicked.
    let mut clicked = false;
    let chosen = |ranked: &[mics::Ranked]| {
        let inputs = inputs.lock().expect("inputs lock poisoned");
        mics::choose(ranked, &inputs).cloned()
    };
    loop {
        let wait = if recording.is_some() || local.is_some() {
            Duration::from_millis(5)
        } else if early.is_some() {
            Duration::from_millis(25)
        } else {
            Duration::from_secs(3600)
        };
        match cmd_rx.recv_timeout(wait) {
            Ok(None) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(Some(Command::Start { uuid })) => {
                // Wispr's own dictation takes the microphone: a local one
                // ends with what it heard.
                if let Some(l) = local.take() {
                    end_local(l, &internal, pill_tx);
                }
                // A new start replaces an unfinished recording, as Wispr's Hub does.
                if let Some(old) = recording.take() {
                    for e in old.stop(clock_ms()) {
                        out.send(&e);
                    }
                }
                let started = match early.take() {
                    Some((mut r, _)) => {
                        r.adopt(uuid.clone());
                        Ok(r)
                    }
                    None => {
                        let mic = chosen(&ranked);
                        start_recording(cfg, &uuid, mic.as_ref(), clock_ms(), out, &mut ready)
                    }
                };
                match started {
                    Ok(r) => {
                        out.send(&Event::Started { uuid });
                        recording = Some(r);
                    }
                    Err(error) => out.send(&Event::MicFailed { uuid, error }),
                }
            }
            Ok(Some(Command::Stop)) => {
                if let Some(r) = recording.take() {
                    for e in r.stop(clock_ms()) {
                        out.send(&e);
                    }
                }
                let _ = relist.send(());
            }
            Ok(Some(Command::Mics { ranked: r })) => {
                ranked = r;
                let _ = relist.send(());
            }
            Ok(Some(Command::Relisted)) => {
                if recording.is_none() && local.is_none() {
                    prepare(&mut ready, &ranked);
                }
            }
            // The shortcut went down, or Wispr's dictation began without it
            // (a click on the bar): start ahead of Wispr's request.
            Ok(Some(Command::Early)) => {
                if recording.is_none() && early.is_none() && local.is_none() {
                    let mic = chosen(&ranked);
                    early = start_early(cfg, mic.as_ref(), clock_ms(), out, &mut ready);
                }
            }
            Ok(Some(Command::EarlyCancel)) => early = None,
            Ok(Some(Command::Status { value })) => {
                match value.as_str() {
                    "initializing" if recording.is_none() && early.is_none() && local.is_none() => {
                        let mic = chosen(&ranked);
                        early = start_early(cfg, mic.as_ref(), clock_ms(), out, &mut ready);
                    }
                    "idle" | "dismissed" | "error" => early = None,
                    _ => {}
                }
                let _ = pill_tx.send(PillCommand::Status(value));
            }
            Ok(Some(Command::Level { value })) => {
                let _ = pill_tx.send(PillCommand::Level(value));
            }
            Ok(Some(Command::Pill { enabled })) => {
                let _ = pill_tx.send(PillCommand::Enabled(enabled));
            }
            Ok(Some(Command::Sound { name })) => play(cfg, &name, out),
            Ok(Some(Command::Mic { active })) => {
                let _ = pill_tx.send(PillCommand::Mic(active));
            }
            Ok(Some(Command::Bar { visible })) => {
                let _ = pill_tx.send(PillCommand::RestingBar(visible));
            }
            Ok(Some(Command::Popo)) => {
                let _ = pill_tx.send(PillCommand::Popo);
            }
            Ok(Some(Command::Silence { value })) => {
                let _ = pill_tx.send(PillCommand::Silence(value));
            }
            Ok(Some(Command::Commanding { active })) => {
                let _ = pill_tx.send(PillCommand::CommandMode(active));
            }
            Ok(Some(Command::Indicator { state })) => {
                let _ = pill_tx.send(PillCommand::Indicator(state));
            }
            Ok(Some(Command::Notify {
                title,
                actions,
                timeout,
            })) => {
                let _ = pill_tx.send(PillCommand::Notify {
                    title,
                    actions,
                    timeout: Duration::from_secs_f64(timeout.max(0.0) / 1000.0),
                });
            }
            Ok(Some(Command::NotifyClear)) => {
                let _ = pill_tx.send(PillCommand::NotifyClear);
            }
            Ok(Some(Command::Dictate { backend: wanted })) => match wanted.unwrap_or(backend) {
                Backend::Wispr => {
                    if !clicked && recording.is_none() && local.is_none() {
                        // A recording the shortcut started waits for
                        // Wispr's request from now on.
                        if let Some((_, at)) = early.as_mut() {
                            *at = Instant::now();
                        }
                        clicked = true;
                        out.send(&Event::Click {
                            action: "start".into(),
                        });
                    }
                }
                // One dictation at a time.
                Backend::Local if recording.is_some() || local.is_some() || clicked => {}
                Backend::Local => match &asr {
                    None => local_failed(out, pill_tx, "no local model (--asr-config)".into()),
                    Some(asr) => {
                        // What the shortcut recorded since it went down.
                        let started = match early.take() {
                            Some((r, _)) => Ok(r),
                            None => {
                                let mic = chosen(&ranked);
                                start_recording(cfg, "", mic.as_ref(), clock_ms(), out, &mut ready)
                            }
                        };
                        match started {
                            Ok(r) => {
                                local = Some(Local {
                                    recording: r,
                                    dictation: asr.start(),
                                });
                                let _ = pill_tx.send(PillCommand::Local);
                                let _ = pill_tx.send(PillCommand::Status("listening".into()));
                                let _ = pill_tx.send(PillCommand::Mic(true));
                            }
                            Err(error) => local_failed(out, pill_tx, error),
                        }
                    }
                },
            },
            Ok(Some(Command::DictateEnd)) => {
                if std::mem::take(&mut clicked) {
                    out.send(&Event::Click {
                        action: "stop".into(),
                    });
                }
                if let Some(l) = local.take() {
                    end_local(l, &internal, pill_tx);
                    let _ = relist.send(());
                }
            }
            Ok(Some(Command::DictateCancel)) => {
                early = None;
                if std::mem::take(&mut clicked) {
                    out.send(&Event::Click {
                        action: "cancel".into(),
                    });
                }
                if local.take().is_some() {
                    let _ = pill_tx.send(PillCommand::Mic(false));
                    let _ = pill_tx.send(PillCommand::Status("idle".into()));
                    let _ = relist.send(());
                }
            }
            Ok(Some(Command::Transcribed { result })) => {
                // The bar belongs to the next dictation if one started meanwhile.
                let quiet = recording.is_none() && local.is_none() && !clicked;
                match result {
                    Ok(text) => {
                        if quiet {
                            let _ = pill_tx.send(PillCommand::Status("idle".into()));
                        }
                        if let Some(typist) = &typist {
                            let _ = typist.send(text.clone());
                        }
                        out.send(&Event::Transcript { text });
                    }
                    Err(error) if quiet => local_failed(out, pill_tx, error),
                    Err(error) => out.send(&Event::TranscriptFailed { error }),
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        // No request to record followed: stop and forget what it heard.
        if early
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() > EARLY_START_PATIENCE)
        {
            early = None;
            let _ = relist.send(());
        }
        if let Some(r) = recording.as_mut() {
            match r.poll(Duration::from_millis(15)) {
                Progress::Chunks(events) => {
                    for e in events {
                        out.send(&e);
                    }
                }
                Progress::Failed(e) => {
                    out.send(&e);
                    recording = None;
                }
            }
        }
        if let Some(l) = local.as_mut() {
            match l.recording.poll_pcm(Duration::from_millis(15)) {
                Ok(pcm) if pcm.is_empty() => {}
                Ok(pcm) => {
                    let _ = pill_tx.send(PillCommand::Level(recorder::volume_db(&pcm)));
                    l.dictation.feed(&pcm);
                }
                Err(error) => {
                    local = None;
                    local_failed(out, pill_tx, error);
                }
            }
        }
    }
    if let Some(r) = recording.take() {
        let _ = r.uuid();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(capture: &str, target: Option<&str>) -> Config {
        Config {
            run: Run::Shim,
            hotkeys: Vec::new(),
            typing: typing::Options::default(),
            capture: capture.split(' ').map(String::from).collect(),
            target: target.map(String::from),
            device: "Default".into(),
            player: Vec::new(),
            sounds: None,
            fonts: None,
            backend: None,
            asr_config: None,
            fill: bar::WISPR_FILL,
            local_fill: bar::LOCAL_FILL,
        }
    }

    fn jabra() -> mics::Native {
        mics::Native {
            id: "alsa_input.jabra".into(),
            name: "Jabra Evolve2 65".into(),
        }
    }

    #[test]
    fn pipewire_records_wispr_s_choice_unless_a_target_is_given() {
        let c = cfg("pw-cat --record --raw", None);
        assert_eq!(capture_for(&c, None).0.join(" "), "pw-cat --record --raw -");
        let (capture, device) = capture_for(&c, Some(&jabra()));
        assert_eq!(
            capture.join(" "),
            "pw-cat --record --raw --target alsa_input.jabra -"
        );
        assert_eq!(device, "Jabra Evolve2 65");
        let c = cfg("pw-cat --record --raw", Some("my-mic"));
        let (capture, device) = capture_for(&c, Some(&jabra()));
        assert_eq!(capture.join(" "), "pw-cat --record --raw --target my-mic -");
        assert_eq!(device, "my-mic");
    }

    #[test]
    fn bar_colours_are_hex_triplets() {
        assert_eq!(color("#c81414"), Some([0xc8, 0x14, 0x14]));
        assert_eq!(color("15803d"), Some([0x15, 0x80, 0x3d]));
        assert_eq!(color("#c8141"), None);
        assert_eq!(color("#gg0000"), None);
        assert_eq!(color("#ä00000"), None);
    }

    #[test]
    fn avfoundation_names_the_chosen_device() {
        let c = cfg("ffmpeg -f avfoundation -i :default -ac 1 -", None);
        assert_eq!(
            capture_for(&c, None).0.join(" "),
            "ffmpeg -f avfoundation -i :default -ac 1 -"
        );
        let mac = mics::Native {
            id: "MacBook Pro Microphone".into(),
            name: "MacBook Pro Microphone".into(),
        };
        let (capture, device) = capture_for(&c, Some(&mac));
        assert_eq!(
            capture[4], ":MacBook Pro Microphone",
            "one argument, spaces and all"
        );
        assert_eq!(device, "MacBook Pro Microphone");
    }
}
