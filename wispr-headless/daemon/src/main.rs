#![forbid(unsafe_code)]
//! Stands in for Wispr Flow's hidden windows while they are not rendered:
//! records the microphone for its dictation and draws the speech indicator.
//! Spawned by the Electron shim (wispr-headless/shim.js), which speaks to it
//! in JSON lines on stdin/stdout. Exits when stdin closes.

mod bar;
#[cfg(target_os = "macos")]
mod pill_mac;
#[cfg(target_os = "linux")]
mod pill_x11;
mod protocol;
mod recorder;
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
    capture: Vec<String>,
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
    if let Some(t) = target.filter(|t| !t.is_empty()) {
        if capture.iter().any(|c| c == "--record") {
            capture.push("--target".into());
            capture.push(t.clone());
        }
        device = t;
    }
    if capture.iter().any(|c| c == "--record") {
        capture.push("-".into());
    }
    Config {
        capture,
        device,
        player,
        sounds,
        fonts,
    }
}

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

    // Commands arrive on their own thread so audio can be polled in between.
    let (cmd_tx, cmd_rx) = mpsc::channel::<Option<Command>>();
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
    loop {
        let wait = if recording.is_some() {
            Duration::from_millis(5)
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
                match Recording::start(uuid.clone(), &cfg.capture, cfg.device.clone(), clock_ms()) {
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
            }
            Ok(Some(Command::Status { value })) => {
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
