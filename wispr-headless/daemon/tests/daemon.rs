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
    // Tests with a real broker explicitly override this private directory.
    // Fixture transcripts must never reach the user's desktop broker.
    _broker_home: tempfile::TempDir,
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
        Self::start_with_env(capture, args, &[])
    }

    fn start_with_env(capture: &str, args: &[&str], env: &[(&str, &str)]) -> Daemon {
        let broker_home = tempfile::tempdir().unwrap();
        let mut child = Command::new(binary())
            .args(["--capture-cmd", capture])
            .args(args)
            .env("DICTATION_API_HOME", broker_home.path())
            .envs(env.iter().copied())
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
            _broker_home: broker_home,
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
            if v["op"] == "log"
                || (v["op"] == "hideKeys" && op != "hideKeys")
                || (v["op"] == "cloudStart" && op != "cloudStart")
                || (v["op"] == "cloudEmpty" && op != "cloudEmpty")
                || (v["op"] == "chunk" && op == "wisprKey")
            {
                continue;
            }
            assert_eq!(v["op"], op, "unexpected message {v}");
            return v;
        }
    }
}

#[test]
fn cloud_capture_queues_while_processing_or_retrying_then_replays_every_sample() {
    let (_tone, path, expected) = tone_file(5000);
    let mut daemon = Daemon::start(&format!("cat {path}"));
    daemon.send(r#"{"op":"dictate","backend":"wispr"}"#);
    assert_eq!(daemon.next_op("wisprKey"), wispr_key("ptt", true));
    daemon.send(r#"{"op":"start","uuid":"first"}"#);
    daemon.next_op("started");
    daemon.send(r#"{"op":"dictateEnd"}"#);
    assert_eq!(daemon.next_op("wisprKey"), wispr_key("ptt", false));
    daemon.send(r#"{"op":"stop"}"#);
    daemon.next_op("done");
    daemon.send(r#"{"op":"status","value":"processing"}"#);
    daemon.send(r#"{"op":"dictate","backend":"wispr"}"#);
    daemon.quiet(100);
    daemon.send(r#"{"op":"dictateEnd"}"#);
    daemon.send(r#"{"op":"cloudState","value":"retrying"}"#);
    daemon.quiet(50);
    daemon.send(r#"{"op":"cloudState","value":"idle"}"#);
    assert_eq!(daemon.next_op("wisprKey"), wispr_key("ptt", true));
    daemon.send(r#"{"op":"start","uuid":"second"}"#);
    daemon.next_op("started");
    let mut audio = Vec::new();
    let mut index = 0;
    loop {
        let event = daemon.next();
        match event["op"].as_str() {
            Some("log") => {}
            Some("chunk") => {
                assert_eq!(event["uuid"], "second");
                assert_eq!(event["index"], index);
                index += 1;
                audio.extend(
                    base64::engine::general_purpose::STANDARD
                        .decode(event["pcm"].as_str().unwrap())
                        .unwrap(),
                );
            }
            Some("wisprKey") => {
                assert_eq!(event, wispr_key("ptt", false));
                daemon.send(r#"{"op":"stop"}"#);
            }
            Some("done") => break,
            _ => panic!("unexpected queue event {event}"),
        }
    }
    assert_eq!(
        audio, expected,
        "all audio recorded while Wispr was busy is replayed exactly once"
    );
}

// The test executable doubles as a model server, so target tests need no
// Python or downloaded model and exercise the real socket/process lifecycle.
#[test]
fn fake_model_server() {
    use std::io::Read;
    use std::os::unix::net::UnixListener;
    let Some(socket) = std::env::var_os("WISPR_TEST_MODEL_SOCKET") else {
        return;
    };
    if let Some(log) = std::env::var_os("WISPR_TEST_MODEL_STARTS") {
        use std::fs::OpenOptions;
        writeln!(
            OpenOptions::new()
                .append(true)
                .create(true)
                .open(log)
                .unwrap(),
            "{}",
            std::process::id()
        )
        .unwrap();
    }
    let listener = UnixListener::bind(socket).unwrap();
    let transcript = serde_json::json!({"text": std::env::var("WISPR_TEST_MODEL_TEXT")
        .unwrap_or_else(|_| "local words".into())})
    .to_string();
    for stream in listener.incoming() {
        let mut stream = stream.unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut first = String::new();
        reader.read_line(&mut first).unwrap();
        let mut length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
            if let Some(n) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = n.trim().parse::<usize>().unwrap();
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        let body = if first.contains("/tokenize") {
            r#"{"tokens":[1]}"#
        } else if first.contains("/transcriptions") {
            if let Some(gate) = std::env::var_os("WISPR_TEST_TRANSCRIPTION_GATE") {
                let until = std::time::Instant::now() + Duration::from_secs(10);
                while !std::path::Path::new(&gate).exists() {
                    assert!(std::time::Instant::now() < until, "transcription gate");
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            std::thread::sleep(Duration::from_millis(200));
            &transcript
        } else {
            "{}"
        };
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
    }
}

fn working_asr_config() -> (tempfile::TempDir, String, String) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let server = dir.path().join("fake-model");
    let starts = dir.path().join("starts");
    let executable = std::env::current_exe().unwrap();
    std::fs::write(&server, format!("#!/bin/sh\nexport WISPR_TEST_MODEL_SOCKET=\"$6\"\nexport WISPR_TEST_MODEL_STARTS='{}'\nexec '{}' --exact fake_model_server --nocapture\n", starts.display(), executable.display())).unwrap();
    std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o755)).unwrap();
    let config = dir.path().join("asr.json");
    std::fs::write(
        &config,
        serde_json::json!({ "server": server, "idleSeconds": 0 }).to_string(),
    )
    .unwrap();
    let config = config.to_string_lossy().into_owned();
    let starts = starts.to_string_lossy().into_owned();
    (dir, config, starts)
}

#[test]
fn local_alternative_inserts_once_and_suppresses_the_later_cloud_result() {
    let (_tone, path, _) = tone_file(16000);
    let (_model, config, _) = working_asr_config();
    let mut daemon = Daemon::start_with(&format!("cat {path}"), &["--asr-config", &config]);
    daemon.send(r#"{"op":"dictate","backend":"wispr"}"#);
    let id = daemon.next_op("cloudStart")["id"].as_u64().unwrap();
    daemon.next_op("wisprKey");
    daemon.send(r#"{"op":"start","uuid":"race"}"#);
    daemon.next_op("started");
    std::thread::sleep(Duration::from_millis(100));
    daemon.send(r#"{"op":"dictateEnd"}"#);
    daemon.next_op("wisprKey");
    daemon.send(r#"{"op":"stop"}"#);
    daemon.next_op("done");
    daemon.send(&format!(r#"{{"op":"localAlternative","id":{id}}}"#));
    assert_eq!(daemon.next_op("insertTranscript")["text"], "local words");
    assert_eq!(daemon.next_op("transcript")["text"], "local words");
    daemon.send(
        r#"{"op":"cloudTranscript","request":"late","uuid":"race","text":"later cloud words"}"#,
    );
    let delivery = daemon.next_op("cloudDelivery");
    assert_eq!(delivery["applied"], true);
    assert_eq!(delivery["fallback"], false);
    daemon.send(r#"{"op":"cloudState","value":"idle"}"#);
    daemon.send(
        r#"{"op":"cloudTranscript","request":"even-later","uuid":"race","text":"duplicate"}"#,
    );
    assert_eq!(daemon.next_op("cloudDelivery")["applied"], true);
}

#[test]
fn overlapping_local_dictations_share_one_model_process() {
    let (_tone, path, _) = tone_file(16000);
    let (_model, config, starts) = working_asr_config();
    let mut daemon = Daemon::start_with(&format!("cat {path}"), &["--asr-config", &config]);
    for _ in 0..2 {
        daemon.send(r#"{"op":"dictate","backend":"local"}"#);
        std::thread::sleep(Duration::from_millis(50));
        daemon.send(r#"{"op":"dictateEnd"}"#);
    }
    for _ in 0..2 {
        assert_eq!(daemon.next_op("insertTranscript")["text"], "local words");
        assert_eq!(daemon.next_op("transcript")["text"], "local words");
    }
    assert_eq!(
        std::fs::read_to_string(starts).unwrap().lines().count(),
        1,
        "the weights are loaded only once"
    );
}

#[test]
fn empty_cloud_result_preserves_an_in_flight_local_attempt() {
    let (_tone, path, _) = tone_file(16000);
    let (model, config, _) = working_asr_config();
    let gate = model.path().join("release");
    let mut daemon = Daemon::start_with_env(
        &format!("cat {path}"),
        &["--asr-config", &config],
        &[("WISPR_TEST_TRANSCRIPTION_GATE", gate.to_str().unwrap())],
    );
    daemon.send(r#"{"op":"dictate","backend":"wispr"}"#);
    let id = daemon.next_op("cloudStart")["id"].as_u64().unwrap();
    daemon.next_op("wisprKey");
    daemon.send(r#"{"op":"start","uuid":"cloud-empty-local-pending"}"#);
    daemon.next_op("started");
    std::thread::sleep(Duration::from_millis(100));
    daemon.send(r#"{"op":"dictateEnd"}"#);
    daemon.next_op("wisprKey");
    daemon.send(r#"{"op":"stop"}"#);
    daemon.next_op("done");
    daemon.send(&format!(r#"{{"op":"localAlternative","id":{id}}}"#));
    daemon.send(r#"{"op":"cloudEmpty","uuid":"cloud-empty-local-pending"}"#);
    assert_eq!(daemon.next_op("cloudEmpty")["id"], id);
    daemon.send(r#"{"op":"cloudState","value":"idle"}"#);
    std::fs::write(gate, "release").unwrap();
    assert_eq!(daemon.next_op("insertTranscript")["text"], "local words");
    assert_eq!(daemon.next_op("transcript")["text"], "local words");
    daemon.send(r#"{"op":"cloudTranscript","request":"duplicate","uuid":"cloud-empty-local-pending","text":"late"}"#);
    assert_eq!(daemon.next_op("cloudDelivery")["applied"], true);
}

#[test]
fn empty_local_alternative_does_not_discard_words_still_processing_in_cloud() {
    let (_tone, path, _) = tone_file(16000);
    let (_model, config, _) = working_asr_config();
    let mut daemon = Daemon::start_with_env(
        &format!("cat {path}"),
        &["--asr-config", &config],
        &[("WISPR_TEST_MODEL_TEXT", "")],
    );
    daemon.send(r#"{"op":"dictate","backend":"wispr"}"#);
    let id = daemon.next_op("cloudStart")["id"].as_u64().unwrap();
    daemon.next_op("wisprKey");
    daemon.send(r#"{"op":"start","uuid":"local-empty-cloud-pending"}"#);
    daemon.next_op("started");
    std::thread::sleep(Duration::from_millis(100));
    daemon.send(r#"{"op":"dictateEnd"}"#);
    daemon.next_op("wisprKey");
    daemon.send(r#"{"op":"stop"}"#);
    daemon.next_op("done");
    daemon.send(&format!(r#"{{"op":"localAlternative","id":{id}}}"#));
    assert_eq!(daemon.next_op("cloudEmpty")["id"], id);
    daemon.send(&format!(r#"{{"op":"localAlternative","id":{id}}}"#));
    daemon.quiet(300);
    daemon.send(r#"{"op":"cloudTranscript","request":"cloud-words","uuid":"local-empty-cloud-pending","text":"actual words"}"#);
    assert_eq!(daemon.next_op("cloudDelivery")["fallback"], true);
}

#[test]
fn cloud_wins_once_while_the_local_alternative_is_still_processing() {
    let (_tone, path, _) = tone_file(16000);
    let (_model, config, _) = working_asr_config();
    let mut daemon = Daemon::start_with(&format!("cat {path}"), &["--asr-config", &config]);
    daemon.send(r#"{"op":"dictate","backend":"wispr"}"#);
    let id = daemon.next_op("cloudStart")["id"].as_u64().unwrap();
    daemon.next_op("wisprKey");
    daemon.send(r#"{"op":"start","uuid":"cloud-wins"}"#);
    daemon.next_op("started");
    std::thread::sleep(Duration::from_millis(100));
    daemon.send(r#"{"op":"dictateEnd"}"#);
    daemon.next_op("wisprKey");
    daemon.send(r#"{"op":"stop"}"#);
    daemon.next_op("done");
    daemon.send(&format!(r#"{{"op":"localAlternative","id":{id}}}"#));
    daemon.send(
        r#"{"op":"cloudTranscript","request":"winner","uuid":"cloud-wins","text":"cloud words"}"#,
    );
    assert_eq!(daemon.next_op("cloudDelivery")["fallback"], true);
    daemon.send(&format!(
        r#"{{"op":"cloudState","value":"idle","id":{id}}}"#
    ));
    daemon.quiet(500);
    daemon.send(r#"{"op":"cloudTranscript","request":"duplicate","uuid":"cloud-wins","text":"cloud words"}"#);
    assert_eq!(daemon.next_op("cloudDelivery")["applied"], true);
}

#[test]
fn a_cancel_before_wispr_starts_never_delivers_or_holds_its_key() {
    let (_tone, path, _) = tone_file(1600);
    let mut daemon = Daemon::start(&format!("cat {path}"));
    daemon.send(r#"{"op":"dictate","backend":"wispr"}"#);
    daemon.next_op("wisprKey");
    daemon.send(r#"{"op":"dictateCancel"}"#);
    assert_eq!(daemon.next_op("click")["action"], "cancel");
    assert_eq!(daemon.next_op("wisprKey"), wispr_key("ptt", false));
    daemon.send(r#"{"op":"start","uuid":"cancelled-before-start"}"#);
    daemon.next_op("started");
    assert_eq!(daemon.next_op("click")["action"], "cancel");
    assert_eq!(daemon.next_op("wisprKey"), wispr_key("ptt", false));
    daemon.send(r#"{"op":"stop"}"#);
    daemon.next_op("done");
    daemon.send(r#"{"op":"cloudTranscript","request":"cancelled","uuid":"cancelled-before-start","text":"discard"}"#);
    assert_eq!(daemon.next_op("cloudDelivery")["applied"], true);
}

/// Run against the SDK's real broker, isolated from the user's service:
/// DICTATION_BROKER_EXE=/path/to/dictation-broker cargo test --test daemon broker_routes -- --ignored
#[test]
#[ignore = "requires DICTATION_BROKER_EXE"]
fn broker_routes_to_the_original_app_and_falls_back_only_on_decline() {
    use dictation_api::{Application, AudioFeatures, Client, Event as BrokerEvent};
    struct Broker(Child);
    impl Drop for Broker {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let home = tempfile::tempdir().unwrap();
    let _broker = Broker(
        Command::new(std::env::var_os("DICTATION_BROKER_EXE").expect("broker executable"))
            .arg("run")
            .env("DICTATION_API_HOME", home.path())
            .env("DICTATION_API_NO_NATIVE_FOCUS", "1")
            .env("DICTATION_API_NO_DBUS", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let endpoint = home.path().join("endpoint.json");
    let until = std::time::Instant::now() + Duration::from_secs(5);
    while !endpoint.exists() {
        assert!(std::time::Instant::now() < until, "broker startup");
        std::thread::sleep(Duration::from_millis(20));
    }
    let application = |id: &str| {
        Application {
        id: id.into(), name: id.into(), process_id: std::process::id(),
        icon: Some("data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' width='8' height='8'><rect width='8' height='8'/></svg>".into()),
    }
    };
    let a = Client::connect(application("a"), Some(&endpoint)).unwrap();
    let b = Client::connect(application("b"), Some(&endpoint)).unwrap();
    let event = |client: &Client| loop {
        let event = client
            .next_event(Duration::from_secs(3))
            .unwrap()
            .expect("broker event");
        if !matches!(event, BrokerEvent::Processing { .. }) {
            break event;
        }
    };
    let offer = |client: &Client| match event(client) {
        BrokerEvent::Offer { session, .. } => session,
        e => panic!("expected offer, got {e:?}"),
    };
    let mut daemon = Daemon::start_with_env(
        "sleep 30",
        &[],
        &[("DICTATION_API_HOME", home.path().to_str().unwrap())],
    );
    a.activate().unwrap();
    daemon.send(r#"{"op":"start","uuid":"direct"}"#);
    daemon.next_op("started");
    let session = offer(&a);
    b.activate().unwrap(); // Changing focus cannot retarget the captured session.
    a.respond(
        &session,
        true,
        AudioFeatures {
            level: true,
            spectrum: true,
        },
    )
    .unwrap();
    daemon.send(r#"{"op":"level","value":0.7}"#);
    assert!(
        matches!(event(&a), BrokerEvent::Audio { frame, .. } if frame.level == Some(0.7) && frame.spectrum.is_none())
    );
    daemon.send(r#"{"op":"stop"}"#);
    daemon.next_op("done");
    daemon.send(r#"{"op":"cloudState","value":"idle"}"#);
    // The next dictation may start before the older transcript arrives.
    daemon.send(r#"{"op":"start","uuid":"declined"}"#);
    daemon.next_op("started");
    b.respond(&offer(&b), false, AudioFeatures::default())
        .unwrap();
    daemon.send(r#"{"op":"stop"}"#);
    daemon.next_op("done");
    daemon.send(r#"{"op":"cloudTranscript","request":"one","uuid":"direct","text":"captured field","submit":true}"#);
    assert!(
        matches!(event(&a), BrokerEvent::Transcript { transcript, .. } if transcript.text == "captured field" && transcript.submit)
    );
    assert!(b.next_event(Duration::from_millis(50)).unwrap().is_none());
    a.acknowledge(&session, true, None).unwrap();
    let result = daemon.next_op("cloudDelivery");
    assert_eq!(result["applied"], true);
    assert_eq!(result["fallback"], false);
    daemon.send(r#"{"op":"cloudState","value":"idle"}"#);

    daemon.send(
        r#"{"op":"cloudTranscript","request":"two","uuid":"declined","text":"ordinary paste"}"#,
    );
    assert_eq!(daemon.next_op("cloudDelivery")["fallback"], true);
    daemon.send(r#"{"op":"status","value":"idle"}"#);

    daemon.send(r#"{"op":"start","uuid":"failed"}"#);
    daemon.next_op("started");
    let session = offer(&b);
    b.respond(&session, true, AudioFeatures::default()).unwrap();
    daemon.send(r#"{"op":"stop"}"#);
    daemon.next_op("done");
    daemon.send(r#"{"op":"cloudTranscript","request":"three","uuid":"failed","text":"keep out of other windows"}"#);
    assert!(matches!(event(&b), BrokerEvent::Transcript { .. }));
    b.acknowledge(&session, false, Some("field destroyed".into()))
        .unwrap();
    let result = daemon.next_op("cloudDelivery");
    assert_eq!(result["applied"], false);
    assert_eq!(result["fallback"], false);
    assert_eq!(result["error"], "field destroyed");

    daemon.send(r#"{"op":"cloudState","value":"idle"}"#);
    daemon.send(r#"{"op":"dictate","backend":"wispr"}"#);
    let busy_id = daemon.next_op("cloudStart")["id"].as_u64().unwrap();
    daemon.next_op("wisprKey");
    let busy = offer(&b);
    b.respond(&busy, true, AudioFeatures::default()).unwrap();
    daemon.send(r#"{"op":"start","uuid":"busy"}"#);
    daemon.next_op("started");
    daemon.send(r#"{"op":"dictateEnd"}"#);
    daemon.next_op("wisprKey");
    daemon.send(r#"{"op":"stop"}"#);
    daemon.next_op("done");
    a.activate().unwrap();
    daemon.send(r#"{"op":"dictate","backend":"wispr"}"#);
    let queued = offer(&a); // Offered now, before Wispr can start this job.
    a.respond(&queued, true, AudioFeatures::default()).unwrap();
    daemon.send(r#"{"op":"dictateEnd"}"#);
    daemon.quiet(50);
    b.activate().unwrap();
    daemon.send(&format!(
        r#"{{"op":"cloudState","value":"idle","id":{busy_id}}}"#
    ));
    daemon.next_op("wisprKey");
    daemon.send(r#"{"op":"start","uuid":"queued"}"#);
    daemon.next_op("started");
    daemon.next_op("wisprKey");
    daemon.send(r#"{"op":"stop"}"#);
    daemon.next_op("done");
    // A stale completion cannot release the newer job's Wispr slot.
    daemon.send(&format!(
        r#"{{"op":"cloudState","value":"idle","id":{busy_id}}}"#
    ));
    daemon.send(r#"{"op":"cloudTranscript","request":"queued-delivery","uuid":"queued","text":"original queued app"}"#);
    assert!(
        matches!(event(&a), BrokerEvent::Transcript { transcript, .. } if transcript.text == "original queued app")
    );
    a.acknowledge(&queued, true, None).unwrap();
    assert_eq!(daemon.next_op("cloudDelivery")["applied"], true);
    // This app also owns the older busy job, whose capture has now ended.
    if let Some(event) = b.next_event(Duration::from_millis(50)).unwrap() {
        assert!(matches!(event, BrokerEvent::Processing { .. }));
    }

    let c = Client::connect(application("lifecycle"), Some(&endpoint)).unwrap();
    c.activate().unwrap();
    let (_model, config, _) = working_asr_config();
    let mut lifecycle = Daemon::start_with_env(
        "sleep 30",
        &["--asr-config", &config],
        &[("DICTATION_API_HOME", home.path().to_str().unwrap())],
    );
    lifecycle.send(r#"{"op":"dictate","backend":"local"}"#);
    let cancelled_local = offer(&c);
    c.respond(&cancelled_local, true, AudioFeatures::default())
        .unwrap();
    lifecycle.send(r#"{"op":"dictateCancel"}"#);
    assert!(
        matches!(event(&c), BrokerEvent::Cancelled { session, .. } if session == cancelled_local)
    );

    lifecycle.send(r#"{"op":"dictate","backend":"wispr"}"#);
    lifecycle.next_op("cloudStart");
    lifecycle.next_op("wisprKey");
    let processing = offer(&c);
    c.respond(&processing, true, AudioFeatures::default())
        .unwrap();
    lifecycle.send(r#"{"op":"start","uuid":"processing-lifecycle"}"#);
    lifecycle.next_op("started");
    lifecycle.send(r#"{"op":"dictateEnd"}"#);
    lifecycle.next_op("wisprKey");
    assert!(
        matches!(c.next_event(Duration::from_secs(3)).unwrap(), Some(BrokerEvent::Processing { session }) if session == processing)
    );
    lifecycle.send(r#"{"op":"stop"}"#);
    lifecycle.next_op("done");

    lifecycle.send(r#"{"op":"dictate","backend":"wispr"}"#);
    let cancelled_queued = offer(&c);
    c.respond(&cancelled_queued, true, AudioFeatures::default())
        .unwrap();
    lifecycle.send(r#"{"op":"dictateCancel"}"#);
    assert!(
        matches!(event(&c), BrokerEvent::Cancelled { session, .. } if session == cancelled_queued)
    );
    lifecycle.send(r#"{"op":"cloudState","value":"dismissed"}"#);
    assert!(matches!(event(&c), BrokerEvent::Cancelled { session, .. } if session == processing));

    lifecycle.send(r#"{"op":"start","uuid":"external-cancel"}"#);
    lifecycle.next_op("started");
    let external = offer(&c);
    c.respond(&external, true, AudioFeatures::default())
        .unwrap();
    lifecycle.send(r#"{"op":"dictateCancel"}"#);
    lifecycle.next_op("click");
    assert!(matches!(event(&c), BrokerEvent::Cancelled { session, .. } if session == external));

    lifecycle.send(r#"{"op":"cloudState","value":"dismissed"}"#);
    lifecycle.send(r#"{"op":"start","uuid":"empty-transcript"}"#);
    lifecycle.next_op("started");
    let empty = offer(&c);
    c.respond(&empty, true, AudioFeatures::default()).unwrap();
    lifecycle.send(r#"{"op":"stop"}"#);
    lifecycle.next_op("done");
    lifecycle
        .send(r#"{"op":"cloudTranscript","request":"empty","uuid":"empty-transcript","text":" "}"#);
    assert!(matches!(event(&c), BrokerEvent::Cancelled { session, .. } if session == empty));
    assert_eq!(lifecycle.next_op("cloudDelivery")["applied"], true);

    // Wispr can finish without ever issuing PasteText. Its exact terminal
    // signal must cancel the accepted route even after its earlier idle.
    lifecycle.send(r#"{"op":"cloudState","value":"idle"}"#);
    lifecycle.send(r#"{"op":"start","uuid":"empty-without-paste"}"#);
    lifecycle.next_op("started");
    let empty = offer(&c);
    c.respond(&empty, true, AudioFeatures::default()).unwrap();
    lifecycle.send(r#"{"op":"stop"}"#);
    lifecycle.next_op("done");
    lifecycle.send(r#"{"op":"cloudState","value":"idle"}"#);
    lifecycle.send(r#"{"op":"cloudEmpty","uuid":"empty-without-paste"}"#);
    lifecycle.next_op("cloudEmpty");
    assert!(matches!(event(&c), BrokerEvent::Cancelled { session, .. } if session == empty));
    lifecycle.send(r#"{"op":"cloudTranscript","request":"retired-empty","uuid":"empty-without-paste","text":"late duplicate"}"#);
    assert_eq!(lifecycle.next_op("cloudDelivery")["applied"], true);

    // A recording with no captured samples also terminates through local
    // recognition, without sending an empty Transcript/submit to the app.
    lifecycle.send(r#"{"op":"dictate","backend":"local"}"#);
    let empty_local = offer(&c);
    c.respond(&empty_local, true, AudioFeatures::default())
        .unwrap();
    lifecycle.send(r#"{"op":"dictateEnd"}"#);
    assert!(matches!(event(&c), BrokerEvent::Cancelled { session, .. } if session == empty_local));
    assert_eq!(lifecycle.next_op("transcript")["text"], "");

    lifecycle.send(r#"{"op":"dictate","backend":"wispr"}"#);
    let id = lifecycle.next_op("cloudStart")["id"].as_u64().unwrap();
    lifecycle.next_op("wisprKey");
    let empty_retry = offer(&c);
    c.respond(&empty_retry, true, AudioFeatures::default())
        .unwrap();
    lifecycle.send(r#"{"op":"start","uuid":"empty-local-retry"}"#);
    lifecycle.next_op("started");
    lifecycle.send(r#"{"op":"dictateEnd"}"#);
    lifecycle.next_op("wisprKey");
    lifecycle.send(r#"{"op":"stop"}"#);
    lifecycle.next_op("done");
    lifecycle.send(r#"{"op":"cloudState","value":"retrying"}"#);
    lifecycle.send(&format!(r#"{{"op":"localAlternative","id":{id}}}"#));
    assert_eq!(lifecycle.next_op("cloudEmpty")["id"], id);
    // The shim stops retrying once the local recognizer confirms no words.
    lifecycle.send(&format!(
        r#"{{"op":"cloudState","value":"empty","id":{id}}}"#
    ));
    lifecycle.next_op("cloudEmpty");
    assert!(matches!(event(&c), BrokerEvent::Cancelled { session, .. } if session == empty_retry));
    lifecycle.send(&format!(r#"{{"op":"localAlternative","id":{id}}}"#));
    lifecycle.quiet(300);

    // The same empty local attempt is terminal when Wispr already went idle
    // without a paste. No follow-up cloud status is needed to clear the app.
    lifecycle.send(r#"{"op":"dictate","backend":"wispr"}"#);
    let id = lifecycle.next_op("cloudStart")["id"].as_u64().unwrap();
    lifecycle.next_op("wisprKey");
    let empty_after_idle = offer(&c);
    c.respond(&empty_after_idle, true, AudioFeatures::default())
        .unwrap();
    lifecycle.send(r#"{"op":"start","uuid":"empty-after-idle"}"#);
    lifecycle.next_op("started");
    lifecycle.send(r#"{"op":"dictateEnd"}"#);
    lifecycle.next_op("wisprKey");
    lifecycle.send(r#"{"op":"stop"}"#);
    lifecycle.next_op("done");
    lifecycle.send(r#"{"op":"cloudState","value":"idle"}"#);
    lifecycle.send(&format!(r#"{{"op":"localAlternative","id":{id}}}"#));
    assert_eq!(lifecycle.next_op("cloudEmpty")["id"], id);
    assert!(
        matches!(event(&c), BrokerEvent::Cancelled { session, .. } if session == empty_after_idle)
    );
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
    let mut d = Daemon::start("sleep 30");
    d.send(r#"{"op":"dictate","backend":"wispr"}"#);
    assert_eq!(d.next_op("wisprKey"), wispr_key("ptt", true));
    d.send(r#"{"op":"start","uuid":"owned"}"#);
    d.next_op("started");
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

fn check_local_stop_sound(hands_free: bool) {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

    let (_tone, path, _) = tone_file(16000);
    let (model, config, _) = working_asr_config();
    let gate = model.path().join("transcription-release");
    let sound_log = model.path().join("sounds");
    let player = model.path().join("player");
    std::fs::write(
        &player,
        "#!/bin/sh\nprintf '%s\\n' \"$1\" >> \"$WISPR_TEST_SOUND_LOG\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&player, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut daemon = Daemon::start_with_env(
        &format!("cat {path}"),
        &[
            "--asr-config",
            &config,
            "--hotkey-source",
            "stdin",
            "--hotkey",
            "local:RightCommand",
            "--player-cmd",
            player.to_str().unwrap(),
            "--local-start-sound",
            "start.wav",
            "--local-stop-sound",
            "stop.wav",
        ],
        &[
            ("WISPR_TEST_TRANSCRIPTION_GATE", gate.to_str().unwrap()),
            ("WISPR_TEST_SOUND_LOG", sound_log.to_str().unwrap()),
        ],
    );
    let sounds = || std::fs::read_to_string(&sound_log).unwrap_or_default();
    let wait_for_sound = |expected: &str, timeout: Duration| {
        let until = Instant::now() + timeout;
        while sounds() != expected {
            assert!(
                Instant::now() < until,
                "expected {expected:?}, got {:?}",
                sounds()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    if hands_free {
        daemon.tap(RIGHT_META);
        std::thread::sleep(Duration::from_millis(80));
    }
    daemon.key(RIGHT_META, true);
    wait_for_sound("start.wav\n", Duration::from_secs(5));
    if hands_free {
        daemon.key(RIGHT_META, false);
        daemon.quiet(100);
        daemon.key(RIGHT_META, true);
    }
    daemon.key(RIGHT_META, false);
    // Recognition cannot finish until released. The cue must still be played
    // promptly by both hold-release and hands-free stop, exactly once.
    wait_for_sound("start.wav\nstop.wav\n", Duration::from_millis(500));
    daemon.quiet(100);
    std::fs::write(gate, "release").unwrap();
    assert_eq!(daemon.next_op("insertTranscript")["text"], "local words");
    assert_eq!(daemon.next_op("transcript")["text"], "local words");
    daemon.quiet(100);
    assert_eq!(sounds(), "start.wav\nstop.wav\n");
}

#[test]
fn local_hold_release_plays_stop_sound_before_transcription_finishes() {
    check_local_stop_sound(false);
}

#[test]
fn local_hands_free_stop_plays_stop_sound_before_transcription_finishes() {
    check_local_stop_sound(true);
}

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
    assert_eq!(
        hidden["keys"],
        serde_json::json!([0xa5]),
        "Right Alt's VK code"
    );
    #[cfg(target_os = "macos")]
    assert_eq!(hidden["keys"], serde_json::json!([61]));
}

#[test]
fn holding_the_wispr_shortcut_holds_wispr_s_push_to_talk_key() {
    let mut d = Daemon::with_keys("sleep 30", &[]);
    d.key(RIGHT_ALT, true);
    // Not before the press is a dictation (200 ms).
    d.quiet(120);
    assert_eq!(d.next_op("wisprKey"), wispr_key("ptt", true));
    d.send(r#"{"op":"start","uuid":"owned"}"#);
    d.next_op("started");
    d.key(RIGHT_ALT, false);
    assert_eq!(d.next_op("wisprKey"), wispr_key("ptt", false));
    // A tap: nothing at all.
    d.tap(RIGHT_ALT);
    d.quiet(700);
}

#[test]
fn a_double_press_presses_wispr_s_hands_free_key_and_any_shortcut_ends_it() {
    let mut d = Daemon::with_keys("sleep 30", &[]);
    d.tap(RIGHT_ALT);
    std::thread::sleep(Duration::from_millis(80));
    d.key(RIGHT_ALT, true);
    assert_eq!(d.next_op("wisprKey"), wispr_key("ptt", true));
    d.send(r#"{"op":"start","uuid":"owned"}"#);
    d.next_op("started");
    d.key(RIGHT_ALT, false);
    // Wispr reports its dictation as it would.
    d.send(r#"{"op":"status","value":"initializing"}"#);
    d.send(r#"{"op":"popo"}"#);
    d.send(r#"{"op":"status","value":"listening"}"#);
    d.quiet(500);
    // The local shortcut stops it, as Wispr's own key would, and starts
    // nothing of its own.
    d.key(RIGHT_META, true);
    assert_eq!(d.next_op("wisprKey"), wispr_key("ptt", false));
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
    d.send(r#"{"op":"start","uuid":"owned"}"#);
    d.next_op("started");
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
    d.send(r#"{"op":"start","uuid":"owned"}"#);
    d.next_op("started");
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
    let mut d = Daemon::with_keys(
        &format!("cat {tone}"),
        &["--asr-config", config.to_str().unwrap()],
    );
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
