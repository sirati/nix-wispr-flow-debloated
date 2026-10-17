//! End to end: a private session bus, a real fcitx5, and the real daemon. The
//! test plays mutter (a libibus client) and checks what arrives.
//!
//! Needs `dbus-daemon` and `fcitx5` binaries; give their paths in
//! `WISPR_E2E_DBUS_DAEMON` and `WISPR_E2E_FCITX5`. Without them the tests
//! print a note and pass, so `cargo test` works anywhere.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use wispr_ime::wire;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::{Connection, MessageStream};

struct Env {
    dbus_daemon: String,
    fcitx5: String,
}

fn env() -> Option<Env> {
    Some(Env {
        dbus_daemon: std::env::var("WISPR_E2E_DBUS_DAEMON").ok()?,
        fcitx5: std::env::var("WISPR_E2E_FCITX5").ok()?,
    })
}

/// Kills its process when dropped, so a failing test leaves nothing running.
struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Session {
    _bus: Guard,
    _fcitx: Guard,
    daemon: Guard,
    address: String,
    config_home: PathBuf,
    _dir: tempfile::TempDir,
}

const PROFILE: &str = "[Groups/0]\nName=Default\nDefault Layout=gb\nDefaultIM=keyboard-gb-intl\n\n\
[Groups/0/Items/0]\nName=keyboard-gb-intl\n\n[Groups/0/Items/1]\nName=keyboard-us\n\n\
[GroupOrder]\n0=Default\n";
const GLOBAL: &str = "[Hotkey/TriggerKeys]\n0=Super+space\n";

async fn start(env: &Env) -> Session {
    let dir = tempfile::tempdir().expect("tempdir");
    let config_home = dir.path().join("config");
    std::fs::create_dir_all(config_home.join("fcitx5")).unwrap();
    std::fs::write(config_home.join("fcitx5/profile"), PROFILE).unwrap();
    std::fs::write(config_home.join("fcitx5/config"), GLOBAL).unwrap();

    let mut bus = Command::new(&env.dbus_daemon)
        .args(["--session", "--nofork", "--print-address=1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("dbus-daemon starts");
    let address = {
        use std::io::{BufRead, BufReader};
        let mut line = String::new();
        BufReader::new(bus.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        line.trim().to_string()
    };
    assert!(address.starts_with("unix:"), "bus address: {address:?}");
    // Without the broker's GUID, like the plain `unix:path=/run/user/N/bus`
    // that sessions use; with it, a replacement broker would be refused.
    let address = address.split(',').next().unwrap_or_default().to_string();
    let bus = Guard(bus);

    let common = |cmd: &mut Command| {
        cmd.env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("DBUS_SESSION_BUS_ADDRESS", &address)
            .env("HOME", dir.path())
            .env("XDG_CONFIG_HOME", &config_home)
            .env("XDG_DATA_HOME", dir.path().join("data"))
            .env("XDG_CACHE_HOME", dir.path().join("cache"))
            .env("XDG_RUNTIME_DIR", dir.path())
            .env("WAYLAND_DISPLAY", "wayland-test")
            .env_remove("DISPLAY");
    };

    let mut fcitx = Command::new(&env.fcitx5);
    common(&mut fcitx);
    fcitx
        .args([
            "--disable",
            "all",
            "--enable",
            "dbus,dbusfrontend,keyboard,inputmethod",
            "-D",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let fcitx = Guard(fcitx.spawn().expect("fcitx5 starts"));

    let mut daemon = Command::new(env!("CARGO_BIN_EXE_wispr-ime"));
    common(&mut daemon);
    daemon
        .env("WISPR_IME_LOG", "debug")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    let daemon = Guard(daemon.spawn().expect("wispr-ime starts"));

    Session {
        _bus: bus,
        _fcitx: fcitx,
        daemon,
        address,
        config_home,
        _dir: dir,
    }
}

async fn client(address: &str) -> Connection {
    zbus::connection::Builder::address(address)
        .expect("address")
        .build()
        .await
        .expect("client connects")
}

async fn wait_until<F: FnMut() -> bool>(what: &str, mut ok: F) {
    for _ in 0..200 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn has_owner(conn: &Connection, name: &str) -> bool {
    let bus = zbus::fdo::DBusProxy::new(conn).await.unwrap();
    bus.name_has_owner(name.try_into().unwrap())
        .await
        .unwrap_or(false)
}

async fn wait_for_name(conn: &Connection, name: &str) {
    for _ in 0..300 {
        if has_owner(conn, name).await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{name} never appeared");
}

/// Everything the client received from the input context, in order.
#[derive(Default)]
struct Received {
    commits: Vec<String>,
    signals: Vec<String>,
}

fn listen(conn: &Connection) -> Arc<Mutex<Received>> {
    let received = Arc::new(Mutex::new(Received::default()));
    let sink = received.clone();
    let mut stream = MessageStream::from(conn);
    tokio::spawn(async move {
        while let Some(Ok(msg)) = stream.next().await {
            let header = msg.header();
            if header.interface().map(|i| i.as_str()) != Some("org.freedesktop.IBus.InputContext") {
                continue;
            }
            let Some(member) = header.member().map(|m| m.to_string()) else {
                continue;
            };
            let mut r = sink.lock().unwrap();
            r.signals.push(member.clone());
            if member == "CommitText" {
                if let Ok((v,)) = msg.body().deserialize::<(OwnedValue,)>() {
                    if let Some(text) = wire::text_of(&v) {
                        r.commits.push(text);
                    }
                }
            }
        }
    });
    received
}

async fn ibus_call<B>(
    conn: &Connection,
    path: &str,
    member: &str,
    body: &B,
) -> zbus::Result<zbus::Message>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    conn.call_method(
        Some("org.freedesktop.IBus"),
        path,
        Some("org.freedesktop.IBus.InputContext"),
        member,
        body,
    )
    .await
}

/// evdev key codes and X keysyms for the keys the tests press.
const KEY_APOSTROPHE: u32 = 40;
const KEY_A: u32 = 30;
const KEY_SPACE: u32 = 57;
const SYM_APOSTROPHE: u32 = 0x27;
const SYM_A: u32 = 0x61;
const SYM_SPACE: u32 = 0x20;
const SUPER_MASK: u32 = 1 << 26;
const RELEASE: u32 = 1 << 30;

async fn key(conn: &Connection, ic: &str, sym: u32, code: u32, state: u32) -> bool {
    let reply = ibus_call(conn, ic, "ProcessKeyEvent", &(sym, code, state))
        .await
        .expect("ProcessKeyEvent");
    reply.body().deserialize::<(bool,)>().expect("bool reply").0
}

async fn press(conn: &Connection, ic: &str, sym: u32, code: u32) -> bool {
    let down = key(conn, ic, sym, code, 0).await;
    let _up = key(conn, ic, sym, code, RELEASE).await;
    down
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mutter_like_client_gets_dead_keys_and_dictation() {
    let Some(env) = env() else {
        eprintln!("skipped: set WISPR_E2E_DBUS_DAEMON and WISPR_E2E_FCITX5");
        return;
    };
    let session = start(&env).await;
    let conn = client(&session.address).await;

    wait_for_name(&conn, "org.freedesktop.IBus").await;
    wait_for_name(&conn, "org.wisprime.WisprIme").await;

    // The address file GNOME reads must point at this bus and at a live pid.
    let machine = ["/var/lib/dbus/machine-id", "/etc/machine-id"]
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "machine-id".into());
    let file = session
        .config_home
        .join(format!("ibus/bus/{machine}-unix-wayland-test"));
    wait_until("the IBus address file", || file.exists()).await;
    let text = std::fs::read_to_string(&file).unwrap();
    let (written, pid) = wispr_ime::addrfile::parse(&text).expect("parsable address file");
    assert!(
        written.starts_with(&session.address),
        "address {written} vs {}",
        session.address
    );
    assert!(written.contains("fcitx_random_string="));
    assert_eq!(
        pid,
        session.daemon.0.id(),
        "pid in the file is the daemon's"
    );

    let received = listen(&conn);

    // Like mutter: create a context, say what the client supports, focus it.
    let reply = conn
        .call_method(
            Some("org.freedesktop.IBus"),
            "/org/freedesktop/IBus",
            Some("org.freedesktop.IBus"),
            "CreateInputContext",
            &("gnome-shell",),
        )
        .await
        .expect("CreateInputContext");
    let (ic,): (OwnedObjectPath,) = reply.body().deserialize().unwrap();
    let ic = ic.to_string();
    assert!(ic.starts_with("/org/freedesktop/IBus/InputContext_"));
    ibus_call(&conn, &ic, "SetCapabilities", &(1u32 | 32,))
        .await
        .unwrap();
    ibus_call(&conn, &ic, "FocusIn", &()).await.unwrap();

    // Dead acute then a: the intl layout composes "á".
    press(&conn, &ic, SYM_APOSTROPHE, KEY_APOSTROPHE).await;
    press(&conn, &ic, SYM_A, KEY_A).await;
    wait_until("the composed character", || {
        received.lock().unwrap().commits.contains(&"á".to_string())
    })
    .await;

    // The IM switch key is consumed by fcitx5, not typed.
    assert!(
        key(&conn, &ic, SYM_SPACE, KEY_SPACE, SUPER_MASK).await,
        "Super+Space should be handled by fcitx5"
    );
    let _ = key(&conn, &ic, SYM_SPACE, KEY_SPACE, SUPER_MASK | RELEASE).await;

    // The dictation stand-in: commit text into the focused field.
    let voice = |text: &'static str| {
        let conn = conn.clone();
        async move {
            let reply = conn
                .call_method(
                    Some("org.wisprime.WisprIme"),
                    "/org/wisprime/WisprIme",
                    Some("org.wisprime.WisprIme1"),
                    "Commit",
                    &(text,),
                )
                .await
                .expect("Commit");
            reply.body().deserialize::<(bool,)>().unwrap().0
        }
    };
    assert!(voice("").await, "probe: a field has focus");
    let spoken = "hello äöü ß 日本語";
    assert!(voice(spoken).await);
    wait_until("the dictated text", || {
        received.lock().unwrap().commits.iter().any(|c| c == spoken)
    })
    .await;

    // Without focus there is nothing to commit into, and nothing is sent.
    ibus_call(&conn, &ic, "FocusOut", &()).await.unwrap();
    let before = received.lock().unwrap().commits.len();
    assert!(!voice("ignored").await, "no focused field");
    assert!(!voice("").await, "probe: no focused field");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(received.lock().unwrap().commits.len(), before);

    // Another client may not drive this context.
    let intruder = client(&session.address).await;
    let ok = ibus_call(&intruder, &ic, "ProcessKeyEvent", &(SYM_A, KEY_A, 0u32)).await;
    let handled = ok
        .ok()
        .and_then(|m| m.body().deserialize::<(bool,)>().ok())
        .map(|b| b.0);
    assert_eq!(handled, Some(false), "a foreign caller is ignored");

    // Clean shutdown retracts the address file.
    let pid = session.daemon.0.id();
    let status = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    wait_until("the address file to be blanked", || {
        std::fs::read_to_string(&file)
            .map(|t| wispr_ime::addrfile::parse(&t).is_none())
            .unwrap_or(false)
    })
    .await;
}

/// Waits for the daemon to exit by itself and returns its exit status.
async fn exit_status_within(daemon: &mut Guard, seconds: u64) -> Option<std::process::ExitStatus> {
    for _ in 0..seconds * 10 {
        if let Ok(Some(status)) = daemon.0.try_wait() {
            return Some(status);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    None
}

/// A daemon whose bus is replaced must not stay alive pretending: it exits
/// with a failure, so systemd starts a fresh one on the new bus.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_exits_when_the_session_bus_dies() {
    let Some(env) = env() else {
        eprintln!("skipped: set WISPR_E2E_DBUS_DAEMON and WISPR_E2E_FCITX5");
        return;
    };
    let mut session = start(&env).await;
    let conn = client(&session.address).await;
    wait_for_name(&conn, "org.wisprime.WisprIme").await;
    drop(conn);

    session._bus.0.kill().expect("kill the bus");
    let status = exit_status_within(&mut session.daemon, 30).await;
    let status = status.expect("the daemon is still running on a dead bus");
    assert!(
        !status.success(),
        "it must exit with a failure so systemd restarts it: {status:?}"
    );
}

/// A restarted fcitx5 knows none of the daemon's input contexts; the daemon
/// must start over instead of forwarding into the void.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_exits_when_fcitx5_goes_away() {
    let Some(env) = env() else {
        eprintln!("skipped: set WISPR_E2E_DBUS_DAEMON and WISPR_E2E_FCITX5");
        return;
    };
    let mut session = start(&env).await;
    let conn = client(&session.address).await;
    wait_for_name(&conn, "org.wisprime.WisprIme").await;
    drop(conn);

    session._fcitx.0.kill().expect("kill fcitx5");
    let status = exit_status_within(&mut session.daemon, 15).await;
    let status = status.expect("the daemon kept running after fcitx5 vanished");
    assert!(!status.success(), "{status:?}");
}

/// The failure seen on the laptop: the session bus is replaced, but the old
/// broker lives on, so the daemon's connection stays open and error-free while
/// nobody else can reach it. Only comparing bus ids notices.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_exits_when_the_bus_is_replaced_but_its_connection_stays_up() {
    let Some(env) = env() else {
        eprintln!("skipped: set WISPR_E2E_DBUS_DAEMON and WISPR_E2E_FCITX5");
        return;
    };
    let mut session = start(&env).await;
    let conn = client(&session.address).await;
    wait_for_name(&conn, "org.wisprime.WisprIme").await;
    drop(conn);

    // Take over the old broker's socket path with a new broker. The old one
    // keeps running and keeps its existing connections.
    let path = session
        .address
        .strip_prefix("unix:path=")
        .and_then(|rest| rest.split(',').next())
        .expect("a path address")
        .to_string();
    std::fs::remove_file(&path).expect("unlink the old socket");
    let replacement = Command::new(&env.dbus_daemon)
        .args([
            "--session",
            "--nofork",
            "--nopidfile",
            &format!("--address=unix:path={path}"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("second dbus-daemon starts");
    let _replacement = Guard(replacement);
    wait_until("the replacement bus", || {
        std::path::Path::new(&path).exists()
    })
    .await;

    let status = exit_status_within(&mut session.daemon, 40).await;
    let status = status.expect("the daemon stayed attached to the replaced bus");
    assert!(!status.success(), "{status:?}");
}
