//! Runs the real binary with a known "microphone" and checks what it reports.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use base64::Engine;
use serde_json::Value;

struct Daemon {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<Value>,
}

/// The binary under test. Cross-built tests run on another machine than the
/// build, so its path can be given there.
fn binary() -> String {
    std::env::var("WISPR_HEADLESS_BIN").unwrap_or_else(|_| env!("CARGO_BIN_EXE_wispr-headless").into())
}

impl Daemon {
    fn start(capture: &str) -> Daemon {
        let mut child = Command::new(binary())
            .args(["--capture-cmd", capture])
            .env_remove("DISPLAY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("daemon starts");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    if tx.send(v).is_err() {
                        break;
                    }
                }
            }
        });
        let mut d = Daemon {
            child,
            stdin,
            lines,
        };
        assert_eq!(d.next_op("ready")["op"], "ready");
        d
    }

    fn send(&mut self, json: &str) {
        writeln!(self.stdin, "{json}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn next(&self) -> Value {
        self.lines
            .recv_timeout(Duration::from_secs(10))
            .expect("daemon answered")
    }

    /// Next message with this op, skipping log lines.
    fn next_op(&mut self, op: &str) -> Value {
        loop {
            let v = self.next();
            if v["op"] == "log" {
                continue;
            }
            assert_eq!(v["op"], op, "unexpected message {v}");
            return v;
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn tone_file(samples: usize) -> (tempfile::TempDir, String, Vec<u8>) {
    let pcm: Vec<u8> = (0..samples)
        .flat_map(|i| (((i as f64 * 0.05).sin() * 8_000.0) as i16).to_le_bytes())
        .collect();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tone.raw");
    std::fs::write(&path, &pcm).unwrap();
    let p = path.to_string_lossy().into_owned();
    (dir, p, pcm)
}

#[test]
fn records_a_known_tone_in_order_and_reports_timing() {
    // 1 s plus a bit: 25 whole chunks and a partial one.
    let (_dir, path, pcm) = tone_file(16_000 + 100);
    let mut d = Daemon::start(&format!("cat {path}"));
    d.send(r#"{"op":"start","uuid":"session-1"}"#);
    assert_eq!(d.next_op("started")["uuid"], "session-1");

    let mut got = Vec::new();
    let mut indices = Vec::new();
    while indices.len() < 25 {
        let c = d.next_op("chunk");
        assert_eq!(c["uuid"], "session-1");
        indices.push(c["index"].as_u64().unwrap());
        assert_eq!(c["duration"], 40.0);
        let v = c["volume"].as_f64().unwrap();
        assert!(v > -20.0 && v < -9.0, "volume {v}");
        got.extend(
            base64::engine::general_purpose::STANDARD
                .decode(c["pcm"].as_str().unwrap())
                .unwrap(),
        );
    }
    d.send(r#"{"op":"stop"}"#);
    // The partial chunk comes last, then the report.
    let tail = d.next_op("chunk");
    indices.push(tail["index"].as_u64().unwrap());
    got.extend(
        base64::engine::general_purpose::STANDARD
            .decode(tail["pcm"].as_str().unwrap())
            .unwrap(),
    );
    assert_eq!(tail["duration"], 100.0 / 16.0);

    let done = d.next_op("done");
    assert_eq!(done["uuid"], "session-1");
    let t = &done["timing"];
    assert_eq!(t["micAcquisitionSucceeded"], true);
    assert!(t["recordingTime"].as_f64().unwrap() > 0.0);
    assert!(t["absoluteStopTime"].as_f64().unwrap() > 1.7e12);
    assert_eq!(t["audioDeviceName"], "test source");

    assert_eq!(
        indices,
        (0..indices.len() as u64).collect::<Vec<_>>(),
        "indices are consecutive from 0"
    );
    assert_eq!(got, pcm, "audio arrives unchanged");
}

#[test]
fn a_second_session_starts_counting_at_zero() {
    let (_dir, path, _) = tone_file(2_000);
    let mut d = Daemon::start(&format!("cat {path}"));
    for uuid in ["a", "b"] {
        d.send(&format!(r#"{{"op":"start","uuid":"{uuid}"}}"#));
        d.next_op("started");
        let first = d.next_op("chunk");
        assert_eq!(first["index"], 0);
        assert_eq!(first["uuid"], uuid);
        d.send(r#"{"op":"stop"}"#);
        loop {
            let v = d.next();
            if v["op"] == "done" {
                assert_eq!(v["uuid"], uuid);
                break;
            }
        }
    }
}

#[test]
fn no_audio_is_reported_as_a_mic_failure() {
    let mut d = Daemon::start("true");
    d.send(r#"{"op":"start","uuid":"silent"}"#);
    d.next_op("started");
    let f = d.next_op("micFailed");
    assert_eq!(f["uuid"], "silent");
}

#[test]
fn a_missing_capture_program_is_reported_as_a_mic_failure() {
    let mut d = Daemon::start("/nonexistent/capture");
    d.send(r#"{"op":"start","uuid":"x"}"#);
    let f = d.next_op("micFailed");
    assert_eq!(f["uuid"], "x");
    assert!(f["error"].as_str().unwrap().contains("cannot run"));
}

#[test]
fn closing_stdin_ends_the_process() {
    let mut d = Daemon::start("true");
    // Replace stdin with a pipe end that is already closed.
    let closed = Command::new("true")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap()
        .stdin
        .take()
        .unwrap();
    drop(std::mem::replace(&mut d.stdin, closed));
    for _ in 0..50 {
        if d.child.try_wait().unwrap().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("daemon kept running after stdin closed");
}
