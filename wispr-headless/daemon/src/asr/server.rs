//! llama.cpp's server, started for one dictation (or a few, see
//! `idleSeconds`) and stopped afterwards, so the model (1.5 GB and more) is
//! only in memory while it is used. It listens on a Unix socket in a
//! directory only this user can open; requests are plain HTTP/1.1, one per
//! connection.
//!
//! A small shell script runs it, in a process group of its own. Dropping
//! the Server kills the group (SIGKILL: nothing to save, and no waiting for
//! a load to finish). The script also kills the server once its stdin, a
//! pipe from this process, closes, which covers this process dying without
//! dropping the Server; a copy of the pipe that leaked into another child
//! (on macOS a pipe is made close-on-exec only after it is made) can keep
//! that from happening, so it is not the only way. The script exits when the
//! server does, which is how a server that dies is noticed.
//!
//! There is at most one server of each Kind per process: Slot (mod.rs)
//! hands it from dictation to dictation, and spawning one stops any other
//! of its kind it finds. The speech model's server and the cleanup model's
//! (cleanup.rs) may run at the same time.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;

use super::Settings;

/// Which model a server has; there is at most one server of each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Granite Speech, with its audio encoder.
    Speech = 0,
    /// The text model that cleans up the transcript (cleanup.rs).
    Cleanup = 1,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Speech => "llama-server",
            Kind::Cleanup => "the cleanup llama-server",
        }
    }
}

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
/// How long a new server waits for the processes of others to go.
const STOP_PATIENCE: Duration = Duration::from_secs(3);
/// Lines of the server's output kept for error messages.
const LOG_LINES: usize = 12;

pub struct Server {
    group: Arc<Group>,
    /// The watchdog's stdin. Only None while dropping.
    stdin: Option<ChildStdin>,
    dir: PathBuf,
    socket: PathBuf,
    log: Arc<Mutex<VecDeque<String>>>,
    /// Tokens of context it was started with.
    pub context: u32,
    kind: Kind,
}

/// What `Server::complete` got back.
#[derive(Debug, Clone, PartialEq)]
pub struct Completion {
    pub text: String,
    /// It stopped at `n_predict` tokens rather than at the end of the text.
    pub at_limit: bool,
}

/// The watchdog's process group: the watchdog, llama-server and anything
/// either started. The watchdog leads it, so the group has its pid.
struct Group {
    pgid: Pid,
    watchdog: Mutex<Watchdog>,
}

struct Watchdog {
    child: Child,
    /// Set once it is reaped; from then on its pid may name another process
    /// (group), so no more signals.
    status: Option<ExitStatus>,
    /// Its Server was dropped, or another server stopped it.
    killed: bool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Group {
    /// SIGKILL to each process in the group.
    fn kill(&self) {
        let mut w = lock(&self.watchdog);
        w.killed = true;
        if w.status.is_none() {
            let _ = killpg(self.pgid, Signal::SIGKILL);
        }
    }

    /// The watchdog's exit status, if it ended (and so reaps it).
    fn exited(&self) -> Option<ExitStatus> {
        let mut w = lock(&self.watchdog);
        if w.status.is_none() {
            w.status = w.child.try_wait().ok().flatten();
        }
        w.status
    }

    /// Nothing of the group is left. Once the watchdog is reaped, a group
    /// with its pid is still ours as long as any of it lives: the pid is not
    /// given out again until then.
    fn gone(&self) -> bool {
        self.exited().is_some() && killpg(self.pgid, None).is_err()
    }

    fn killed(&self) -> bool {
        lock(&self.watchdog).killed
    }
}

/// For each Kind, the groups of all servers of that kind this process
/// started that may still have a process, stopped or not.
static GROUPS: [Mutex<Vec<Arc<Group>>>; 2] = [Mutex::new(Vec::new()), Mutex::new(Vec::new())];
/// One spawn of each kind at a time, so two never miss each other.
static SPAWNING: [Mutex<()>; 2] = [Mutex::new(()), Mutex::new(())];

/// Kills the servers of `kind` still running, as there is to be only one,
/// and waits a little for them and for those stopped before to go.
fn stop_others(kind: Kind, log: &dyn Fn(String)) {
    let groups = &GROUPS[kind as usize];
    let others: Vec<Arc<Group>> = {
        let mut groups = lock(groups);
        groups.retain(|g| !g.gone());
        groups.clone()
    };
    if others.is_empty() {
        return;
    }
    let running = others.iter().filter(|g| !g.killed()).count();
    if running > 0 {
        log(format!(
            "stopping {running} {} still running before starting another",
            kind.name()
        ));
    }
    for g in &others {
        g.kill();
    }
    let deadline = Instant::now() + STOP_PATIENCE;
    while others.iter().any(|g| !g.gone()) {
        if Instant::now() > deadline {
            log(format!(
                "an old {} is still there after {} s; starting another anyway",
                kind.name(),
                STOP_PATIENCE.as_secs()
            ));
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    lock(groups).retain(|g| !g.gone());
}

/// Tests that start servers, one at a time: a start stops the others.
#[cfg(test)]
pub fn one_at_a_time() -> MutexGuard<'static, ()> {
    static TESTS: Mutex<()> = Mutex::new(());
    lock(&TESTS)
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
    /// Starts loading Granite Speech; `ready` waits for it. Stops any other
    /// speech server first.
    pub fn spawn(
        settings: &Settings,
        context: u32,
        log: &dyn Fn(String),
    ) -> Result<Server, String> {
        let model = [
            "--model".into(),
            expand(&settings.model),
            "--mmproj".into(),
            expand(&settings.mmproj),
        ];
        // Read by patch 0003 in ../../../../llama-cpp.
        let coreml = settings.coreml.as_ref().map(|c| {
            let frames = c.frames.iter().map(u32::to_string);
            [
                ("MTMD_COREML_AUDIO", expand(&c.encoder).into_os_string()),
                (
                    "MTMD_COREML_AUDIO_FRAMES",
                    frames.collect::<Vec<_>>().join(",").into(),
                ),
            ]
        });
        launch(
            Kind::Speech,
            &settings.server,
            &model,
            &settings.server_args,
            coreml.as_ref().map_or(&[][..], |c| &c[..]),
            context,
            log,
        )
    }

    /// Starts loading the cleanup model (`settings.cleanup`, which must be
    /// set) with the same llama-server. Stops any other cleanup server first.
    pub fn spawn_cleanup(
        settings: &Settings,
        context: u32,
        log: &dyn Fn(String),
    ) -> Result<Server, String> {
        let cleanup = settings.cleanup.as_ref().ok_or("no cleanup model")?;
        launch(
            Kind::Cleanup,
            &settings.server,
            &["--model".into(), expand(&cleanup.model)],
            &cleanup.server_args,
            &[],
            context,
            log,
        )
    }
}

/// Starts llama-server (`program`) with `model` (its arguments) under the
/// watchdog, after stopping any other server of `kind`.
fn launch(
    kind: Kind,
    program: &str,
    model: &[PathBuf],
    more: &[String],
    env: &[(&str, std::ffi::OsString)],
    context: u32,
    log: &dyn Fn(String),
) -> Result<Server, String> {
    let _one = lock(&SPAWNING[kind as usize]);
    stop_others(kind, log);
    let dir = private_dir()?;
    let socket = dir.join("llama.sock");
    let program = expand(program);
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(WATCHDOG)
        .arg("sh")
        .arg(&program)
        .args(model)
        .arg("--host")
        .arg(&socket)
        // Ignored for a socket, but must not be 0.
        .args(["--port", "1"])
        .args(["--ctx-size", &context.to_string()])
        .args(["--parallel", "1", "--n-gpu-layers", "99", "--temp", "0"])
        // Warming up only delays the first request; no copies of the
        // prompt cache in RAM (one slot keeps its own).
        .args(["--no-warmup", "--cache-ram", "0"])
        .args(more)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot run {program}: {e}", program = program.display()))?;
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
    let stdin = child.stdin.take();
    let group = Arc::new(Group {
        pgid: Pid::from_raw(child.id() as i32),
        watchdog: Mutex::new(Watchdog {
            child,
            status: None,
            killed: false,
        }),
    });
    lock(&GROUPS[kind as usize]).push(group.clone());
    Ok(Server {
        group,
        stdin,
        dir,
        socket,
        log,
        context,
        kind,
    })
}

impl Server {
    /// Why the server is gone, if it is: its exit status and last lines.
    pub fn ended(&mut self) -> Option<String> {
        let status = self.group.exited()?;
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
        self.tokens_within(text, REQUEST_PATIENCE)
    }

    /// The same, giving up after `patience`.
    pub fn tokens_within(&self, text: &str, patience: Duration) -> Result<u32, String> {
        let body = serde_json::json!({ "content": text }).to_string();
        let (status, body) = exchange(
            &self.socket,
            "POST",
            "/tokenize",
            Some("application/json"),
            body.as_bytes(),
            patience,
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

    /// Greedy continuation of `prompt` (llama-server's `/completion`), at
    /// most `n_predict` tokens, giving up after `patience`.
    pub fn complete(
        &mut self,
        prompt: &str,
        n_predict: u32,
        patience: Duration,
    ) -> Result<Completion, String> {
        complete(&self.socket, prompt, n_predict, patience).map_err(|e| self.ended().unwrap_or(e))
    }
}

/// `/completion` with a plain-text prompt and greedy decoding.
fn complete(
    socket: &Path,
    prompt: &str,
    n_predict: u32,
    patience: Duration,
) -> Result<Completion, String> {
    let body = serde_json::json!({
        "prompt": prompt,
        "n_predict": n_predict,
        "temperature": 0,
        "cache_prompt": false,
    })
    .to_string();
    let (status, body) = exchange(
        socket,
        "POST",
        "/completion",
        Some("application/json"),
        body.as_bytes(),
        patience,
    )?;
    let v: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| format!("completion: bad answer ({status}): {e}"))?;
    match (status, v["content"].as_str()) {
        (200, Some(text)) => Ok(Completion {
            text: text.to_string(),
            at_limit: v["stop_type"] == "limit"
                || v["tokens_predicted"]
                    .as_u64()
                    .is_some_and(|n| n >= u64::from(n_predict)),
        }),
        _ => Err(format!(
            "completion failed ({status}): {}",
            v["error"]["message"].as_str().unwrap_or("no message")
        )),
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
        drop(self.stdin.take());
        self.group.kill();
        // Reap the watchdog without making the caller wait.
        let (group, kind) = (self.group.clone(), self.kind);
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            while !group.gone() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if group.gone() {
                lock(&GROUPS[kind as usize]).retain(|g| !Arc::ptr_eq(g, &group));
            }
        });
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

/// One request on a new connection: the status and body of the answer,
/// within `patience` in all.
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
    let deadline = Instant::now() + patience;
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(format!("llama-server {path}: no answer in {patience:.1?}"));
        }
        let _ = stream.set_read_timeout(Some(left));
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(io(e)),
        }
    }
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
pub mod tests {
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

    pub fn answer(status: &str, body: &str) -> String {
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
        let rx = answer_on(&socket, answer);
        (dir, socket, rx)
    }

    /// Listens on `socket`, answers each request with `answer` and hands
    /// what it got to the test.
    pub fn answer_on(socket: &Path, answer: String) -> std::sync::mpsc::Receiver<Vec<u8>> {
        serve_on(socket, move |_| answer.clone())
    }

    /// Listens on `socket`, answers each request with what `answer` makes
    /// of it (on a thread of its own, so it may take its time) and hands
    /// what it got to the test.
    pub fn serve_on(
        socket: &Path,
        answer: impl Fn(&[u8]) -> String + Send + Sync + 'static,
    ) -> std::sync::mpsc::Receiver<Vec<u8>> {
        let answer = Arc::new(answer);
        let listener = UnixListener::bind(socket).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let (tx, answer) = (tx.clone(), answer.clone());
                std::thread::spawn(move || {
                    // Read the head, then as much body as it announces.
                    let mut got = Vec::new();
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = s.read(&mut buf) {
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
                    let reply = answer(&got);
                    let _ = tx.send(got);
                    let _ = s.write_all(reply.as_bytes());
                });
            }
        });
        rx
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
            coreml: None,
            keywords: None,
            idle_seconds: 0.0,
            server_args: Vec::new(),
            target_seconds: 5.0,
            max_seconds: 10.0,
            cleanup: None,
        }
    }

    pub fn script(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-llama-server");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Whether process `pid` runs (a zombie does not).
    pub fn alive(pid: i32) -> bool {
        nix::sys::signal::kill(Pid::from_raw(pid), None).is_ok() && !zombie(pid)
    }

    #[cfg(target_os = "linux")]
    fn zombie(pid: i32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| Some(s.rsplit_once(')')?.1.trim_start().starts_with('Z')))
            .unwrap_or(false)
    }

    #[cfg(not(target_os = "linux"))]
    fn zombie(pid: i32) -> bool {
        Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .is_ok_and(|o| o.stdout.trim_ascii_start().starts_with(b"Z"))
    }

    /// The pid a stand-in server wrote to `path`.
    fn pid_in(path: &Path) -> i32 {
        for _ in 0..1_000 {
            if let Ok(pid) = std::fs::read_to_string(path)
                .unwrap_or_default()
                .trim()
                .parse()
            {
                return pid;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the stand-in server did not start");
    }

    fn wait_gone(pid: i32, what: &str) {
        for _ in 0..1_000 {
            if !alive(pid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("{what}");
    }

    fn no_log(_: String) {}

    #[test]
    fn dropping_the_server_kills_it() {
        let _one = one_at_a_time();
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let fake = script(
            dir.path(),
            &format!("echo $$ > {}\nexec sleep 30", pidfile.display()),
        );
        let server = Server::spawn(&settings(&fake), 1_024, &no_log).unwrap();
        let pid = pid_in(&pidfile);
        assert!(alive(pid), "the stand-in server runs");
        drop(server);
        wait_gone(pid, "the server outlived its Server");
    }

    #[test]
    fn the_core_ml_encoder_is_passed_to_the_speech_server() {
        let _one = one_at_a_time();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("env");
        let fake = script(
            dir.path(),
            &format!(
                "echo \"$MTMD_COREML_AUDIO|$MTMD_COREML_AUDIO_FRAMES\" > {}.tmp\n\
                 mv {0}.tmp {0}\nexec sleep 30",
                out.display()
            ),
        );
        let mut s = settings(&fake);
        s.coreml = Some(super::super::CoreMl {
            encoder: "/m/encoder.mlpackage".into(),
            frames: vec![375, 500],
        });
        let _server = Server::spawn(&s, 1_024, &no_log).unwrap();
        for _ in 0..1_000 {
            if let Ok(env) = std::fs::read_to_string(&out) {
                assert_eq!(env.trim(), "/m/encoder.mlpackage|375,500");
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the stand-in server did not start");
    }

    #[test]
    fn a_leaked_copy_of_its_stdin_does_not_keep_the_server() {
        use std::os::fd::AsFd;
        let _one = one_at_a_time();
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let fake = script(
            dir.path(),
            &format!("echo $$ > {}\nexec sleep 30", pidfile.display()),
        );
        let server = Server::spawn(&settings(&fake), 1_024, &no_log).unwrap();
        // The write end of the watchdog's stdin, as another child started at
        // the same time could have inherited it: here and in a child.
        let leaked = server
            .stdin
            .as_ref()
            .unwrap()
            .as_fd()
            .try_clone_to_owned()
            .unwrap();
        let mut holder = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::from(leaked.try_clone().unwrap()))
            .spawn()
            .unwrap();
        let pid = pid_in(&pidfile);
        assert!(alive(pid), "the stand-in server runs");
        drop(server);
        wait_gone(pid, "the server outlived its Server");
        let _ = holder.kill();
        let _ = holder.wait();
        drop(leaked);
    }

    #[test]
    fn starting_a_server_stops_another() {
        let _one = one_at_a_time();
        let dir = tempfile::tempdir().unwrap();
        let fake = script(dir.path(), "echo $$ > \"$2\"\nexec sleep 30");
        let (first, second) = (dir.path().join("1"), dir.path().join("2"));
        let mut s = settings(&fake);
        s.model = first.to_string_lossy().into_owned();
        let mut one = Server::spawn(&s, 1_024, &no_log).unwrap();
        let first = pid_in(&first);
        s.model = second.to_string_lossy().into_owned();
        let logged = Mutex::new(Vec::new());
        let _two = Server::spawn(&s, 1_024, &|m| logged.lock().unwrap().push(m)).unwrap();
        assert!(!alive(first), "the first server was stopped first");
        assert!(alive(pid_in(&second)));
        assert_eq!(
            logged.lock().unwrap().len(),
            1,
            "{:?}",
            logged.lock().unwrap()
        );
        assert!(one.ended().is_some(), "the first one knows");
    }

    #[test]
    fn a_server_that_dies_while_loading_is_reported_with_its_output() {
        let _one = one_at_a_time();
        let dir = tempfile::tempdir().unwrap();
        let fake = script(dir.path(), "echo \"error: unknown model $2\"; exit 3");
        let mut server = Server::spawn(&settings(&fake), 1_024, &no_log).unwrap();
        let e = server.ready(&|| false).unwrap_err();
        assert!(e.contains("unknown model /nonexistent/model.gguf"), "{e}");
    }

    #[test]
    fn a_missing_server_is_reported() {
        let _one = one_at_a_time();
        let mut server = Server::spawn(
            &settings(Path::new("/nonexistent/llama-server")),
            1_024,
            &no_log,
        )
        .unwrap();
        let e = server.ready(&|| false).unwrap_err();
        assert!(e.contains("ended"), "{e}");
    }

    #[test]
    fn loading_stops_when_cancelled() {
        let _one = one_at_a_time();
        let dir = tempfile::tempdir().unwrap();
        let fake = script(dir.path(), "exec sleep 30");
        let mut server = Server::spawn(&settings(&fake), 1_024, &no_log).unwrap();
        let start = Instant::now();
        assert_eq!(
            server
                .ready(&|| start.elapsed() > Duration::from_millis(100))
                .unwrap_err(),
            "cancelled"
        );
    }
}
