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
    std::env::var("WISPR_HEADLESS_BIN")
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_wispr-headless").into())
}

impl Daemon {
    fn start(capture: &str) -> Daemon {
        Daemon::start_with(capture, &[])
    }

    fn start_with(capture: &str, args: &[&str]) -> Daemon {
        let mut child = Command::new(binary())
            .args(["--capture-cmd", capture])
            .args(args)
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
            if v["op"] == "log" || (v["op"] == "hideKeys" && op != "hideKeys") {
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

/// Everything decoded from the chunks of one session, until its report.
fn collect_session(d: &mut Daemon, uuid: &str) -> (Vec<u8>, Vec<u64>) {
    let mut got = Vec::new();
    let mut indices = Vec::new();
    loop {
        let v = d.next();
        match v["op"].as_str() {
            Some("chunk") => {
                assert_eq!(v["uuid"], uuid);
                indices.push(v["index"].as_u64().unwrap());
                got.extend(
                    base64::engine::general_purpose::STANDARD
                        .decode(v["pcm"].as_str().unwrap())
                        .unwrap(),
                );
            }
            Some("done") => return (got, indices),
            Some("log") => {}
            other => panic!("unexpected message {other:?}: {v}"),
        }
    }
}

#[test]
fn recording_starts_when_the_dictation_does_and_keeps_that_audio() {
    let (_dir, path, pcm) = tone_file(2_000);
    let mut d = Daemon::start(&format!("cat {path}"));
    // Wispr's dictation begins; its request to record comes a bit later.
    d.send(r#"{"op":"status","value":"initializing"}"#);
    std::thread::sleep(Duration::from_millis(150));
    d.send(r#"{"op":"start","uuid":"early-1"}"#);
    assert_eq!(d.next_op("started")["uuid"], "early-1");
    std::thread::sleep(Duration::from_millis(100));
    d.send(r#"{"op":"stop"}"#);
    let (got, indices) = collect_session(&mut d, "early-1");
    assert_eq!(got, pcm, "the audio from before the request is all there");
    assert_eq!(indices, (0..indices.len() as u64).collect::<Vec<_>>());
}

#[test]
fn an_early_start_without_a_request_is_dropped() {
    let (_dir, path, pcm) = tone_file(2_000);
    let mut d = Daemon::start(&format!("cat {path}"));
    d.send(r#"{"op":"status","value":"initializing"}"#);
    // Longer than the daemon waits for the request: nothing may come out.
    std::thread::sleep(Duration::from_millis(800));
    assert!(
        d.lines.try_recv().is_err(),
        "an unclaimed early start sends nothing"
    );
    // A later dictation records afresh.
    d.send(r#"{"op":"start","uuid":"late-1"}"#);
    assert_eq!(d.next_op("started")["uuid"], "late-1");
    std::thread::sleep(Duration::from_millis(100));
    d.send(r#"{"op":"stop"}"#);
    let (got, _) = collect_session(&mut d, "late-1");
    assert_eq!(got, pcm);
}

/// A local model whose server never gets ready.
fn sleeping_asr_config() -> (tempfile::TempDir, String) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let server = dir.path().join("llama-server");
    std::fs::write(&server, "#!/bin/sh\nexec sleep 30\n").unwrap();
    std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = dir.path().join("asr.json");
    std::fs::write(&path, format!(r#"{{"server":"{}"}}"#, server.display())).unwrap();
    let p = path.to_string_lossy().into_owned();
    (dir, p)
}

/// A local model whose server cannot start.
fn broken_asr_config() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("asr.json");
    std::fs::write(&path, r#"{"server":"/nonexistent/llama-server"}"#).unwrap();
    let p = path.to_string_lossy().into_owned();
    (dir, p)
}

#[test]
fn a_dictation_of_our_own_for_wispr_holds_its_push_to_talk_key() {
    let mut d = Daemon::start("true");
    d.send(r#"{"op":"dictate","backend":"wispr"}"#);
    assert_eq!(d.next_op("wisprKey"), wispr_key("ptt", true));
    d.send(r#"{"op":"dictateEnd"}"#);
    assert_eq!(d.next_op("wisprKey"), wispr_key("ptt", false));
}

fn wispr_key(action: &str, down: bool) -> Value {
    serde_json::json!({"op": "wisprKey", "action": action, "down": down})
}

#[test]
fn a_cancelled_dictation_for_wispr_clicks_its_cancel_then_lets_go() {
    let mut d = Daemon::start("true");
    d.send(r#"{"op":"dictate","backend":"wispr"}"#);
    assert_eq!(d.next_op("wisprKey"), wispr_key("ptt", true));
    d.send(r#"{"op":"dictateCancel"}"#);
    assert_eq!(d.next_op("click")["action"], "cancel");
    assert_eq!(d.next_op("wisprKey"), wispr_key("ptt", false));
}

#[test]
fn a_cancelled_local_dictation_is_not_transcribed() {
    let (_tone_dir, tone, _) = tone_file(16_000);
    let (_dir, config) = sleeping_asr_config();
    let mut d = Daemon::start_with(&format!("cat {tone}"), &["--asr-config", &config]);
    // As the shortcut does it: recording from the key going down.
    d.send(r#"{"op":"early"}"#);
    d.send(r#"{"op":"dictate","backend":"local"}"#);
    std::thread::sleep(Duration::from_millis(100));
    d.send(r#"{"op":"dictateCancel"}"#);
    // The next thing said is about the next dictation, not a transcript.
    d.send(r#"{"op":"dictate","backend":"wispr"}"#);
    assert_eq!(d.next_op("wisprKey"), wispr_key("ptt", true));
}

#[test]
fn without_a_local_model_our_dictations_go_to_wispr() {
    let mut d = Daemon::start("true");
    d.send(r#"{"op":"dictate"}"#);
    assert_eq!(d.next_op("wisprKey"), wispr_key("ptt", true));
}

#[test]
fn a_local_dictation_without_a_model_fails() {
    let mut d = Daemon::start("true");
    d.send(r#"{"op":"dictate","backend":"local"}"#);
    let f = d.next_op("transcriptFailed");
    assert!(
        f["error"].as_str().unwrap().contains("no local model"),
        "{f}"
    );
}

#[test]
fn a_local_dictation_records_and_reports_the_model_s_failure() {
    let (_tone_dir, tone, _) = tone_file(16_000);
    let (_dir, config) = broken_asr_config();
    let mut d = Daemon::start_with(&format!("cat {tone}"), &["--asr-config", &config]);
    // The local model is the default now.
    d.send(r#"{"op":"dictate"}"#);
    std::thread::sleep(Duration::from_millis(100));
    d.send(r#"{"op":"dictateEnd"}"#);
    let f = d.next_op("transcriptFailed");
    assert!(
        f["error"].as_str().unwrap().contains("llama-server ended"),
        "{f}"
    );
}

#[test]
fn wispr_s_own_dictations_are_unchanged_with_a_local_model() {
    let (_tone_dir, tone, pcm) = tone_file(2_000);
    let (_dir, config) = broken_asr_config();
    let mut d = Daemon::start_with(&format!("cat {tone}"), &["--asr-config", &config]);
    d.send(r#"{"op":"start","uuid":"w-1"}"#);
    assert_eq!(d.next_op("started")["uuid"], "w-1");
    std::thread::sleep(Duration::from_millis(100));
    d.send(r#"{"op":"stop"}"#);
    let (got, _) = collect_session(&mut d, "w-1");
    assert_eq!(got, pcm);
}

/// Key codes as the shortcut reads them on this platform.
#[cfg(target_os = "macos")]
const KEYS: (u16, u16, u16) = (61, 54, 53);
#[cfg(not(target_os = "macos"))]
const KEYS: (u16, u16, u16) = (100, 126, 1);
const RIGHT_ALT: u16 = KEYS.0;
const RIGHT_META: u16 = KEYS.1;
const ESCAPE: u16 = KEYS.2;

impl Daemon {
    /// With the shortcut wispr:RightAlt + local:RightCommand fed from stdin.
    fn with_keys(capture: &str, more: &[&str]) -> Daemon {
        let mut args = vec![
            "--hotkey-source",
            "stdin",
            "--hotkey",
            "wispr:RightAlt",
            "--hotkey",
            "local:RightCommand",
            "--log-file",
            "",
        ];
        args.extend_from_slice(more);
        Daemon::start_with(capture, &args)
    }

    fn key(&mut self, key: u16, down: bool) {
        self.send(&format!(r#"{{"op":"key","key":{key},"down":{down}}}"#));
    }

    fn tap(&mut self, key: u16) {
        self.key(key, true);
        std::thread::sleep(Duration::from_millis(60));
        self.key(key, false);
    }

    /// No message but logs for `ms`.
    fn quiet(&mut self, ms: u64) {
        let end = std::time::Instant::now() + Duration::from_millis(ms);
        while let Some(left) = end.checked_duration_since(std::time::Instant::now()) {
            match self.lines.recv_timeout(left) {
                Ok(v) if v["op"] == "log" || v["op"] == "hideKeys" => {}
                Ok(v) => panic!("unexpected message {v}"),
                Err(_) => return,
            }
        }
    }
}

#[test]
fn wispr_s_shortcut_keys_are_hidden_from_wispr() {
    let mut d = Daemon::with_keys("true", &[]);
    let hidden = d.next_op("hideKeys");
    #[cfg(not(target_os = "macos"))]
    assert_eq!(hidden["keys"], serde_json::json!([0xa5]), "Right Alt's VK code");
    #[cfg(target_os = "macos")]
    assert_eq!(hidden["keys"], serde_json::json!([61]));
}

#[test]
fn holding_the_wispr_shortcut_holds_wispr_s_push_to_talk_key() {
    let mut d = Daemon::with_keys("true", &[]);
    d.key(RIGHT_ALT, true);
    // Not before the press is a dictation (200 ms).
    d.quiet(120);
    assert_eq!(d.next_op("wisprKey"), wispr_key("ptt", true));
    d.key(RIGHT_ALT, false);
    assert_eq!(d.next_op("wisprKey"), wispr_key("ptt", false));
    // A tap: nothing at all.
    d.tap(RIGHT_ALT);
    d.quiet(700);
}

#[test]
fn a_double_press_presses_wispr_s_hands_free_key_and_any_shortcut_ends_it() {
    let mut d = Daemon::with_keys("true", &[]);
    d.tap(RIGHT_ALT);
    std::thread::sleep(Duration::from_millis(80));
    d.key(RIGHT_ALT, true);
    assert_eq!(d.next_op("wisprKey"), wispr_key("popo", true));
    assert_eq!(d.next_op("wisprKey"), wispr_key("popo", false));
    d.key(RIGHT_ALT, false);
    // Wispr reports its dictation as it would.
    d.send(r#"{"op":"status","value":"initializing"}"#);
    d.send(r#"{"op":"popo"}"#);
    d.send(r#"{"op":"status","value":"listening"}"#);
    d.quiet(500);
    // The local shortcut stops it, as Wispr's own key would, and starts
    // nothing of its own.
    d.key(RIGHT_META, true);
    assert_eq!(d.next_op("wisprKey"), wispr_key("popo", true));
    assert_eq!(d.next_op("wisprKey"), wispr_key("popo", false));
    std::thread::sleep(Duration::from_millis(400));
    d.key(RIGHT_META, false);
    d.send(r#"{"op":"status","value":"processing"}"#);
    d.send(r#"{"op":"status","value":"idle"}"#);
    d.quiet(600);
}

#[test]
fn escape_cancels_a_local_hands_free_dictation() {
    let (_tone_dir, tone, _) = tone_file(16_000 * 5);
    // A model that never loads: only a cancel ends this without text.
    let (_dir, config) = sleeping_asr_config();
    let mut d = Daemon::with_keys(&format!("cat {tone}"), &["--asr-config", &config]);
    d.tap(RIGHT_META);
    std::thread::sleep(Duration::from_millis(80));
    d.tap(RIGHT_META);
    d.quiet(500);
    d.key(ESCAPE, true);
    d.key(ESCAPE, false);
    // No transcript, no failure: dropped.
    d.quiet(800);
}

#[test]
fn a_local_dictation_ends_itself_at_the_time_limit() {
    let (_tone_dir, tone, _) = tone_file(16_000 * 5);
    let (_dir, config) = broken_asr_config();
    let mut d = Daemon::with_keys(
        &format!("cat {tone}"),
        &["--asr-config", &config, "--max-dictation-seconds", "0.5"],
    );
    d.tap(RIGHT_META);
    std::thread::sleep(Duration::from_millis(80));
    d.tap(RIGHT_META);
    // The model cannot start, so the dictation fails; the time limit or the
    // failure ends it, and the next press is a new dictation.
    let f = d.next_op("transcriptFailed");
    assert!(f["error"].as_str().unwrap().contains("llama-server"), "{f}");
    d.key(RIGHT_ALT, true);
    assert_eq!(d.next_op("wisprKey"), wispr_key("ptt", true));
    d.key(RIGHT_ALT, false);
    assert_eq!(d.next_op("wisprKey"), wispr_key("ptt", false));
}

#[test]
fn the_log_file_records_the_dictations() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("logs/wispr-headless.log");
    let mut d = Daemon::start_with(
        "true",
        &[
            "--hotkey-source",
            "stdin",
            "--hotkey",
            "wispr:RightAlt",
            "--log-file",
            log.to_str().unwrap(),
        ],
    );
    d.key(RIGHT_ALT, true);
    assert_eq!(d.next_op("wisprKey"), wispr_key("ptt", true));
    d.key(RIGHT_ALT, false);
    assert_eq!(d.next_op("wisprKey"), wispr_key("ptt", false));
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.contains("started (pid"), "{text}");
    assert!(text.contains("shortcut: down"), "{text}");
    assert!(text.contains("control: Start"), "{text}");
}

/// Whether `pid` runs (a zombie does not count).
fn running(pid: &str) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => !stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
        Err(_) => false,
    }
}

#[cfg(target_os = "linux")]
#[test]
fn spamming_both_shortcuts_never_runs_two_model_servers() {
    use std::os::unix::fs::PermissionsExt;
    let (_tone_dir, tone, _) = tone_file(16_000 * 30);
    let dir = tempfile::tempdir().unwrap();
    let pids = dir.path().join("pids");
    std::fs::create_dir(&pids).unwrap();
    let server = dir.path().join("llama-server");
    std::fs::write(
        &server,
        format!("#!/bin/sh\necho > {}/$$\nexec sleep 60\n", pids.display()),
    )
    .unwrap();
    std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o755)).unwrap();
    let config = dir.path().join("asr.json");
    std::fs::write(&config, format!(r#"{{"server":"{}"}}"#, server.display())).unwrap();
    let mut d = Daemon::with_keys(&format!("cat {tone}"), &["--asr-config", config.to_str().unwrap()]);
    let alive = || {
        std::fs::read_dir(&pids)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| running(&e.file_name().to_string_lossy()))
            .count()
    };
    let mut most = 0;
    let mut seed: u64 = 0x1234_5678_9abc_def1;
    let mut rand = move |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n
    };
    let keys = [RIGHT_META, RIGHT_META, RIGHT_ALT, ESCAPE];
    let mut held = std::collections::BTreeSet::new();
    let end = std::time::Instant::now() + Duration::from_secs(4);
    while std::time::Instant::now() < end {
        let key = keys[rand(keys.len() as u64) as usize];
        let down = held.insert(key);
        if !down {
            held.remove(&key);
        }
        d.key(key, down);
        std::thread::sleep(Duration::from_millis(rand(300)));
        most = most.max(alive());
    }
    for key in held {
        d.key(key, false);
    }
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(50));
        most = most.max(alive());
    }
    assert!(most <= 1, "{most} model servers at once");
    assert!(
        std::fs::read_dir(&pids).unwrap().count() > 0,
        "the spam started local dictations"
    );
}
