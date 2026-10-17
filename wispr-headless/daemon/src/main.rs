#![forbid(unsafe_code)]
//! Stands in for Wispr Flow's hidden windows while they are not rendered:
//! records the microphone for its dictation and draws the speech indicator.
//! Spawned by the Electron shim (wispr-headless/shim.js), which speaks to it
//! in JSON lines on stdin/stdout. Exits when stdin closes.

mod pill;
mod protocol;
mod recorder;

use std::io::{BufRead, Write};
use std::process::{Command as Process, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pill::PillCommand;
use protocol::{Command, Event};
use recorder::{Progress, Recording};

struct Config {
    capture: Vec<String>,
    device: String,
    player: Vec<String>,
    sounds: Option<String>,
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
            // Tests: any command that writes 16 kHz s16 mono PCM to stdout.
            "--capture-cmd" => {
                if let Some(c) = args.next() {
                    capture = c.split_whitespace().map(String::from).collect();
                    device = "test source".into();
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
    let clock = Instant::now();
    let clock_ms = || clock.elapsed().as_secs_f64() * 1_000.0;

    // The flow bar runs on its own thread; its clicks go straight to the shim.
    let (pill_tx, pill_rx) = mpsc::channel::<PillCommand>();
    {
        let log_out = Out(out.0.clone());
        let click_out = Out(out.0.clone());
        std::thread::spawn(move || {
            pill::run(
                pill_rx,
                move |msg| log_out.send(&Event::Log { msg }),
                move |action| {
                    click_out.send(&Event::Click {
                        action: action.to_string(),
                    })
                },
            )
        });
    }

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
            Ok(Some(Command::Sound { name })) => play(&cfg, &name, &out),
            Ok(Some(Command::Bar { visible })) => {
                let _ = pill_tx.send(PillCommand::RestingBar(visible));
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
