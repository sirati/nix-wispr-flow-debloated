//! llama.cpp's server, started for one dictation (or a few, see
//! `idleSeconds`) and stopped afterwards, so the model (1.5 GB and more) is
//! only in memory while it is used. It listens on a Unix socket in a
//! directory only this user can open; requests are plain HTTP/1.1, one per
//! connection.
//!
//! A small shell script runs it and kills it (SIGKILL: nothing to save, and
//! no waiting for a load to finish) once its stdin, a pipe from this
//! process, closes: when the Server is dropped, and also when this process
//! dies without dropping it. It exits when the server does, which is how a
//! server that dies while loading is noticed.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::Settings;

/// Runs the server ("$@") and kills it when stdin closes. A background
/// list reads /dev/null unless told otherwise, hence fd 3.
const WATCHDOG: &str = r#"exec 3<&0
"$@" </dev/null 3<&- 2>&1 &
server=$!
{ read -r _ <&3; kill -9 "$server"; } 2>/dev/null &
reader=$!
exec 3<&-
wait "$server"
status=$?
kill "$reader" 2>/dev/null
exit "$status""#;

/// How long loading may take (a cold disk, a busy machine).
const LOAD_PATIENCE: Duration = Duration::from_secs(120);
/// How long one request may take.
const REQUEST_PATIENCE: Duration = Duration::from_secs(120);
/// Lines of the server's output kept for error messages.
const LOG_LINES: usize = 12;

pub struct Server {
    /// The watchdog script; its stdin is the pipe that keeps the server up.
    /// Only None while dropping.
    child: Option<Child>,
    dir: PathBuf,
    socket: PathBuf,
    log: Arc<Mutex<VecDeque<String>>>,
    /// Tokens of context it was started with.
    pub context: u32,
}

/// `~/` at the start of a configured path means the home directory.
pub fn expand(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => Path::new(&home).join(rest),
        _ => PathBuf::from(path),
    }
}

/// A new private directory for the socket.
fn private_dir() -> Result<PathBuf, String> {
    static COUNT: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "wispr-asr-{}-{}",
        std::process::id(),
        COUNT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    Ok(dir)
}

impl Server {
    /// Starts loading the model; `ready` waits for it.
    pub fn spawn(settings: &Settings, context: u32) -> Result<Server, String> {
        let dir = private_dir()?;
        let socket = dir.join("llama.sock");
        let program = expand(&settings.server);
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(WATCHDOG)
            .arg("sh")
            .arg(&program)
            .arg("--model")
            .arg(expand(&settings.model))
            .arg("--mmproj")
            .arg(expand(&settings.mmproj))
            .arg("--host")
            .arg(&socket)
            // Ignored for a socket, but must not be 0.
            .args(["--port", "1"])
            .args(["--ctx-size", &context.to_string()])
            .args(["--parallel", "1", "--n-gpu-layers", "99", "--temp", "0"])
            // Warming up only delays the first request; no copies of the
            // prompt cache in RAM (one slot keeps its own).
            .args(["--no-warmup", "--cache-ram", "0"])
            .args(&settings.server_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("cannot run {}: {e}", program.display()))?;
        let log = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(out) = child.stdout.take() {
            let log = log.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(out).lines() {
                    let Ok(line) = line else { break };
                    let mut log = log.lock().expect("log lock poisoned");
                    if log.len() == LOG_LINES {
                        log.pop_front();
                    }
                    log.push_back(line);
                }
            });
        }
        Ok(Server {
            child: Some(child),
            dir,
            socket,
            log,
            context,
        })
    }

    /// Why the server is gone, if it is: its exit status and last lines.
    fn ended(&mut self) -> Option<String> {
        let status = self.child.as_mut()?.try_wait().ok()??;
        // Its last words may still be on their way.
        std::thread::sleep(Duration::from_millis(50));
        let log = self.log.lock().expect("log lock poisoned");
        let tail = log.iter().cloned().collect::<Vec<_>>().join(" | ");
        Some(format!("llama-server ended ({status}): {tail}"))
    }

    /// Waits until the model is loaded, the server died, or `cancelled`.
    pub fn ready(&mut self, cancelled: &dyn Fn() -> bool) -> Result<(), String> {
        let deadline = Instant::now() + LOAD_PATIENCE;
        loop {
            if cancelled() {
                return Err("cancelled".into());
            }
            if let Some(why) = self.ended() {
                return Err(why);
            }
            if let Ok((200, _)) = exchange(
                &self.socket,
                "GET",
                "/health",
                None,
                &[],
                Duration::from_secs(1),
            ) {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err(format!(
                    "llama-server did not load in {} s",
                    LOAD_PATIENCE.as_secs()
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Which server this is.
    #[cfg(test)]
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// How many tokens `text` takes.
    pub fn tokens(&self, text: &str) -> Result<u32, String> {
        let body = serde_json::json!({ "content": text }).to_string();
        let (status, body) = exchange(
            &self.socket,
            "POST",
            "/tokenize",
            Some("application/json"),
            body.as_bytes(),
            REQUEST_PATIENCE,
        )?;
        let v: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| format!("tokenize: bad answer ({status}): {e}"))?;
        v["tokens"]
            .as_array()
            .map(|t| t.len() as u32)
            .ok_or_else(|| format!("tokenize failed ({status}): {v}"))
    }

    /// The text of 16 kHz mono `pcm`.
    pub fn transcribe(
        &mut self,
        pcm: &[i16],
        prompt: &str,
        max_tokens: u32,
    ) -> Result<String, String> {
        transcribe(&self.socket, pcm, prompt, max_tokens).map_err(|e| self.ended().unwrap_or(e))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
        // Closing its stdin makes the watchdog kill the server; reap it
        // without making the caller wait.
        if let Some(mut child) = self.child.take() {
            drop(child.stdin.take());
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

/// `/v1/audio/transcriptions` with `pcm` as a WAV file.
fn transcribe(socket: &Path, pcm: &[i16], prompt: &str, max_tokens: u32) -> Result<String, String> {
    let boundary = format!(
        "wispr-headless-{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let body = multipart(
        &boundary,
        &[
            ("prompt", prompt),
            ("temperature", "0"),
            ("max_tokens", &max_tokens.to_string()),
        ],
        &wav(pcm),
    );
    let (status, body) = exchange(
        socket,
        "POST",
        "/v1/audio/transcriptions",
        Some(&format!("multipart/form-data; boundary={boundary}")),
        &body,
        REQUEST_PATIENCE,
    )?;
    let v: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| format!("transcription: bad answer ({status}): {e}"))?;
    match (status, v["text"].as_str()) {
        (200, Some(text)) => Ok(text.trim().to_string()),
        _ => Err(format!(
            "transcription failed ({status}): {}",
            v["error"]["message"].as_str().unwrap_or("no message")
        )),
    }
}

/// A form with text fields and the audio as its `file`.
fn multipart(boundary: &str, fields: &[(&str, &str)], wav: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(wav.len() + 1024);
    for (name, value) in fields {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\nContent-Type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(wav);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

/// 16 kHz mono 16-bit PCM as a WAV file.
pub fn wav(pcm: &[i16]) -> Vec<u8> {
    let data = (pcm.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&16_000u32.to_le_bytes());
    out.extend_from_slice(&32_000u32.to_le_bytes()); // bytes a second
    out.extend_from_slice(&2u16.to_le_bytes()); // bytes a frame
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data.to_le_bytes());
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// One request on a new connection: the status and body of the answer.
fn exchange(
    socket: &Path,
    method: &str,
    path: &str,
    content_type: Option<&str>,
    body: &[u8],
    patience: Duration,
) -> Result<(u16, Vec<u8>), String> {
    let mut stream =
        UnixStream::connect(socket).map_err(|e| format!("cannot reach llama-server: {e}"))?;
    let _ = stream.set_read_timeout(Some(patience));
    let _ = stream.set_write_timeout(Some(patience));
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(t) = content_type {
        head.push_str(&format!("Content-Type: {t}\r\n"));
    }
    head.push_str("\r\n");
    let io = |e: std::io::Error| format!("llama-server {path}: {e}");
    stream.write_all(head.as_bytes()).map_err(io)?;
    stream.write_all(body).map_err(io)?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(io)?;
    parse_response(&raw)
}

/// Status and body of an HTTP/1.1 response read to its end.
fn parse_response(raw: &[u8]) -> Result<(u16, Vec<u8>), String> {
    let end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("llama-server: incomplete answer")?;
    let head = String::from_utf8_lossy(&raw[..end]);
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or("llama-server: no status")?;
    let mut chunked = false;
    let mut length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value.eq_ignore_ascii_case("chunked");
        } else if name.eq_ignore_ascii_case("content-length") {
            length = value.parse::<usize>().ok();
        }
    }
    let rest = &raw[end + 4..];
    let body = if chunked {
        unchunk(rest)?
    } else {
        match length {
            Some(n) if n <= rest.len() => rest[..n].to_vec(),
            Some(_) => return Err("llama-server: answer cut short".into()),
            None => rest.to_vec(),
        }
    };
    Ok((status, body))
}

/// A chunked body ("Transfer-Encoding: chunked") put together.
fn unchunk(mut rest: &[u8]) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    loop {
        let line_end = rest
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or("llama-server: bad chunk")?;
        let size_text = String::from_utf8_lossy(&rest[..line_end]);
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| "llama-server: bad chunk size")?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            return Ok(body);
        }
        if rest.len() < size + 2 {
            return Err("llama-server: chunk cut short".into());
        }
        body.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn wav_header_says_16_khz_mono_16_bit() {
        let w = wav(&[1, -1, 300]);
        assert_eq!(w.len(), 44 + 6);
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(w[4..8].try_into().unwrap()), 36 + 6);
        assert_eq!(&w[8..16], b"WAVEfmt ");
        assert_eq!(u16::from_le_bytes([w[22], w[23]]), 1, "channels");
        assert_eq!(u32::from_le_bytes(w[24..28].try_into().unwrap()), 16_000);
        assert_eq!(u16::from_le_bytes([w[34], w[35]]), 16, "bits");
        assert_eq!(&w[36..40], b"data");
        assert_eq!(u32::from_le_bytes(w[40..44].try_into().unwrap()), 6);
        assert_eq!(&w[44..], &[1, 0, 0xff, 0xff, 0x2c, 0x01]);
    }

    #[test]
    fn form_has_the_fields_then_the_file() {
        let body = multipart(
            "XX",
            &[("prompt", "hi there"), ("temperature", "0")],
            b"RIFFdata",
        );
        let text = String::from_utf8_lossy(&body);
        assert_eq!(
            text,
            "--XX\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\nhi there\r\n\
             --XX\r\nContent-Disposition: form-data; name=\"temperature\"\r\n\r\n0\r\n\
             --XX\r\nContent-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\n\
             Content-Type: audio/wav\r\n\r\nRIFFdata\r\n--XX--\r\n"
        );
    }

    #[test]
    fn answers_with_a_length_or_in_chunks() {
        let plain = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 4\r\n\r\n{}{}extra";
        assert_eq!(parse_response(plain).unwrap(), (200, b"{}{}".to_vec()));
        let chunked = b"HTTP/1.1 503 Service Unavailable\r\ntransfer-encoding: chunked\r\n\r\n3\r\nabc\r\n2;x=y\r\nde\r\n0\r\n\r\n";
        assert_eq!(parse_response(chunked).unwrap(), (503, b"abcde".to_vec()));
        let until_close = b"HTTP/1.1 200 OK\r\n\r\nbody";
        assert_eq!(
            parse_response(until_close).unwrap(),
            (200, b"body".to_vec())
        );
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nshort").is_err());
        assert!(parse_response(b"HTTP/1.1 200").is_err());
    }

    #[test]
    fn home_relative_paths_are_expanded() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand("~/k.txt"), Path::new(&home).join("k.txt"));
        assert_eq!(expand("/abs/~/x"), PathBuf::from("/abs/~/x"));
    }

    fn answer(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    /// A stand-in llama-server on a socket: answers each request with
    /// `answer` and hands what it got to the test.
    fn fake_server(
        answer: String,
    ) -> (
        tempfile::TempDir,
        PathBuf,
        std::sync::mpsc::Receiver<Vec<u8>>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("s.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                // Read the head, then as much body as it announces.
                let mut got = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let n = s.read(&mut buf).unwrap();
                    got.extend_from_slice(&buf[..n]);
                    if let Some(end) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&got[..end]).to_lowercase();
                        let len: usize = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .map(|v| v.trim().parse().unwrap())
                            .unwrap_or(0);
                        if got.len() >= end + 4 + len {
                            break;
                        }
                    }
                    if n == 0 {
                        break;
                    }
                }
                let _ = tx.send(got);
                let _ = s.write_all(answer.as_bytes());
            }
        });
        (dir, socket, rx)
    }

    #[test]
    fn transcription_posts_a_form_and_reads_the_text() {
        let (_dir, socket, got) = fake_server(answer(
            "200 OK",
            r#"{"type":"transcript.text.done","text":" hi "}"#,
        ));
        let text = transcribe(&socket, &[0; 1600], "transcribe the speech to text.", 45).unwrap();
        assert_eq!(text, "hi");
        let request = got.recv().unwrap();
        let head = String::from_utf8_lossy(&request[..200]);
        assert!(
            head.starts_with("POST /v1/audio/transcriptions HTTP/1.1\r\n"),
            "{head}"
        );
        assert!(head.contains("multipart/form-data; boundary=wispr-headless-"));
        let all = String::from_utf8_lossy(&request);
        assert!(all.contains("name=\"prompt\"\r\n\r\ntranscribe the speech to text.\r\n"));
        assert!(all.contains("name=\"max_tokens\"\r\n\r\n45\r\n"));
        assert!(all.contains("RIFF"));
    }

    #[test]
    fn a_refused_transcription_reports_the_server_s_message() {
        let (_dir, socket, _got) = fake_server(answer(
            "400 Bad Request",
            r#"{"error":{"message":"context too long"}}"#,
        ));
        let e = transcribe(&socket, &[0; 160], "p", 1).unwrap_err();
        assert!(e.contains("400") && e.contains("context too long"), "{e}");
    }

    fn settings(server: &Path) -> Settings {
        Settings {
            server: server.to_string_lossy().into_owned(),
            model: "/nonexistent/model.gguf".into(),
            mmproj: "/nonexistent/mmproj.gguf".into(),
            keywords: None,
            idle_seconds: 0.0,
            server_args: Vec::new(),
            target_seconds: 5.0,
            max_seconds: 10.0,
        }
    }

    fn script(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-llama-server");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn alive(pid: &str) -> bool {
        Command::new("/bin/sh")
            .args(["-c", &format!("kill -0 {pid}")])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    }

    #[test]
    fn dropping_the_server_kills_it() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let fake = script(
            dir.path(),
            &format!("echo $$ > {}\nexec sleep 30", pidfile.display()),
        );
        let server = Server::spawn(&settings(&fake), 1_024).unwrap();
        let mut pid = String::new();
        for _ in 0..200 {
            pid = std::fs::read_to_string(&pidfile)
                .unwrap_or_default()
                .trim()
                .to_string();
            if !pid.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(alive(&pid), "the stand-in server runs");
        drop(server);
        for _ in 0..200 {
            if !alive(&pid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the server outlived its Server");
    }

    #[test]
    fn a_server_that_dies_while_loading_is_reported_with_its_output() {
        let dir = tempfile::tempdir().unwrap();
        let fake = script(dir.path(), "echo \"error: unknown model $2\"; exit 3");
        let mut server = Server::spawn(&settings(&fake), 1_024).unwrap();
        let e = server.ready(&|| false).unwrap_err();
        assert!(e.contains("unknown model /nonexistent/model.gguf"), "{e}");
    }

    #[test]
    fn a_missing_server_is_reported() {
        let mut server =
            Server::spawn(&settings(Path::new("/nonexistent/llama-server")), 1_024).unwrap();
        let e = server.ready(&|| false).unwrap_err();
        assert!(e.contains("ended"), "{e}");
    }

    #[test]
    fn loading_stops_when_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let fake = script(dir.path(), "exec sleep 30");
        let mut server = Server::spawn(&settings(&fake), 1_024).unwrap();
        let start = Instant::now();
        assert_eq!(
            server
                .ready(&|| start.elapsed() > Duration::from_millis(100))
                .unwrap_err(),
            "cancelled"
        );
    }
}
