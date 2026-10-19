// Safe Rust, except mic_mac.rs: objc2 marks AVFoundation calls unsafe until
// they are reviewed upstream, so that module allows it (see its header).
#![deny(unsafe_code)]
//! Stands in for Wispr Flow's hidden windows while they are not rendered:
//! records the microphone for its dictation and draws the speech indicator.
//! Spawned by the Electron shim (wispr-headless/shim.js), which speaks to it
//! in JSON lines on stdin/stdout. Exits when stdin closes.

mod bar;
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
mod wave;

#[cfg(target_os = "macos")]
use pill_mac as pill;
#[cfg(target_os = "linux")]
use pill_x11 as pill;

use std::io::{BufRead, Write};
use std::process::{Command as Process, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bar::{BarEvent, PillCommand};
use protocol::{Command, Event};
use recorder::{Progress, Recording};

struct Config {
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
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--pw-cat" => {
                if let Some(p) = args.next() {
                    capture[0] = p.clone();
                    player[0] = p;
                }
            }
            "--target" => target = args.next(),
            "--sounds" => sounds = args.next(),
            "--font-dir" => fonts = args.next(),
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
        capture,
        target: target.filter(|t| !t.is_empty()),
        device,
        player,
        sounds,
        fonts,
    }
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

fn main() {
    let cfg = config();
    let out = Out(Arc::new(Mutex::new(std::io::stdout())));

    // The flow bar keeps the main thread (AppKit needs it on macOS); commands
    // and the recorder run on their own, and the process ends with them.
    let (pill_tx, pill_rx) = mpsc::channel::<PillCommand>();
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
    let chosen = |ranked: &[mics::Ranked]| {
        let inputs = inputs.lock().expect("inputs lock poisoned");
        mics::choose(ranked, &inputs).cloned()
    };
    loop {
        let wait = if recording.is_some() {
            Duration::from_millis(5)
        } else if early.is_some() {
            Duration::from_millis(25)
        } else {
            Duration::from_secs(3600)
        };
        match cmd_rx.recv_timeout(wait) {
            Ok(None) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(Some(Command::Start { uuid })) => {
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
                if recording.is_none() {
                    prepare(&mut ready, &ranked);
                }
            }
            Ok(Some(Command::Status { value })) => {
                match value.as_str() {
                    "initializing" if recording.is_none() && early.is_none() => {
                        let mic = chosen(&ranked);
                        if let Ok(r) =
                            start_recording(cfg, "", mic.as_ref(), clock_ms(), out, &mut ready)
                        {
                            early = Some((r, Instant::now()));
                        }
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
            capture: capture.split(' ').map(String::from).collect(),
            target: target.map(String::from),
            device: "Default".into(),
            player: Vec::new(),
            sounds: None,
            fonts: None,
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
