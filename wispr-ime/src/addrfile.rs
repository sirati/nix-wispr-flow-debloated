//! The IBus address file.
//!
//! libibus (and so mutter) looks for the input method in
//! `$XDG_CONFIG_HOME/ibus/bus/<machine-id>-<host>-<display>`, a two-line file:
//!
//! ```text
//! IBUS_ADDRESS=unix:path=/run/user/1000/bus,fcitx_random_string=...
//! IBUS_DAEMON_PID=1234
//! ```
//!
//! It ignores the file when the pid is not alive, which is why a stale file
//! from a dead `ibus-daemon` leaves GNOME without an input method. The extra
//! `fcitx_random_string` parameter changes the address on every write so
//! clients that cached the old one reconnect; D-Bus address parsers skip
//! parameters they do not know. The naming rules mirror fcitx5's IBus frontend.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// Everything the path rules read from the environment, passed in so the
/// rules can be tested.
#[derive(Debug, Default, Clone)]
pub struct Env {
    pub ibus_address_file: Option<String>,
    pub wayland_display: Option<String>,
    pub display: Option<String>,
    pub config_home: PathBuf,
    pub machine_id: String,
}

impl Env {
    pub fn from_process() -> Env {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let config_home = var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| var("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from(".config"));
        Env {
            ibus_address_file: var("IBUS_ADDRESS_FILE"),
            wayland_display: var("WAYLAND_DISPLAY"),
            display: var("DISPLAY"),
            config_home,
            machine_id: machine_id(),
        }
    }
}

fn machine_id() -> String {
    for path in ["/var/lib/dbus/machine-id", "/etc/machine-id"] {
        if let Ok(text) = fs::read_to_string(path) {
            let id = text.trim();
            if !id.is_empty() {
                return id.to_string();
            }
        }
    }
    "machine-id".to_string()
}

/// File name for one display, e.g. `<id>-unix-wayland-0` or `<id>-unix-1`.
fn file_name(machine_id: &str, wayland: bool, env: &Env) -> String {
    let mut host = "unix".to_string();
    let mut number = "0".to_string();
    if wayland {
        number = "wayland-0".to_string();
        if let Some(display) = &env.wayland_display {
            if let Some(name) = Path::new(display).file_name().and_then(|n| n.to_str()) {
                number = name.to_string();
            }
        }
    } else if let Some(display) = &env.display {
        match display.split_once(':') {
            Some((h, rest)) => {
                if !h.is_empty() {
                    host = h.to_string();
                }
                number = rest.split('.').next().unwrap_or("0").to_string();
            }
            None => {
                if !display.is_empty() {
                    host = display.clone();
                }
            }
        }
    }
    format!("{machine_id}-{host}-{number}")
}

/// Every file clients may read: the X11 one, if there is a `DISPLAY`, and the
/// Wayland one. `IBUS_ADDRESS_FILE` overrides both, as it does in libibus.
pub fn paths(env: &Env) -> Vec<PathBuf> {
    if let Some(file) = &env.ibus_address_file {
        return vec![PathBuf::from(file)];
    }
    let dir = env.config_home.join("ibus").join("bus");
    let mut out = Vec::new();
    if env.display.is_some() {
        out.push(dir.join(file_name(&env.machine_id, false, env)));
    }
    let wayland = dir.join(file_name(&env.machine_id, true, env));
    if !out.contains(&wayland) {
        out.push(wayland);
    }
    out
}

/// What goes into the file.
pub fn contents(address: &str, pid: u32, random: &str) -> String {
    let address = address.trim_end_matches(';');
    format!("IBUS_ADDRESS={address},fcitx_random_string={random}\nIBUS_DAEMON_PID={pid}\n")
}

/// The blanked file left behind on exit: present, but pointing nowhere.
pub fn blank() -> &'static str {
    "IBUS_ADDRESS=\nIBUS_DAEMON_PID=\n"
}

/// Reads `(address, pid)` out of an address file.
pub fn parse(text: &str) -> Option<(String, u32)> {
    let mut address = None;
    let mut pid = None;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("IBUS_ADDRESS=") {
            address = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("IBUS_DAEMON_PID=") {
            pid = v.trim().parse().ok();
        }
    }
    match (address, pid) {
        (Some(a), Some(p)) if !a.is_empty() => Some((a, p)),
        _ => None,
    }
}

/// Writes `text` to `path` through a temporary file, so readers never see a
/// half-written file.
pub fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)
}

/// 128 random bits as hex, from the kernel.
pub fn random_hex() -> io::Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// The address and pid we published, so we can tell later whether the files
/// are still ours.
#[derive(Debug, Clone, PartialEq)]
pub struct Published {
    pub address: String,
    pub pid: u32,
}

/// Writes the file for every path. Returns what was written.
pub fn publish(paths: &[PathBuf], session_bus: &str, pid: u32) -> io::Result<Published> {
    let random = random_hex()?;
    let text = contents(session_bus, pid, &random);
    let (address, _) = parse(&text).expect("we wrote a parsable file");
    for path in paths {
        write_atomic(path, &text)?;
    }
    Ok(Published { address, pid })
}

/// True if every file still holds what we wrote.
pub fn still_ours(paths: &[PathBuf], ours: &Published) -> bool {
    paths.iter().all(|p| {
        fs::read_to_string(p)
            .ok()
            .and_then(|t| parse(&t))
            .is_some_and(|(a, pid)| a == ours.address && pid == ours.pid)
    })
}

/// Blanks the files that still hold our address; files someone else has
/// written since are left alone.
pub fn retract(paths: &[PathBuf], ours: &Published) {
    for path in paths {
        let mine = fs::read_to_string(path)
            .ok()
            .and_then(|t| parse(&t))
            .is_some_and(|(a, pid)| a == ours.address && pid == ours.pid);
        if mine {
            let _ = write_atomic(path, blank());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> Env {
        Env {
            ibus_address_file: None,
            wayland_display: Some("wayland-0".into()),
            display: Some(":1".into()),
            config_home: PathBuf::from("/home/u/.config"),
            machine_id: "abc".into(),
        }
    }

    #[test]
    fn names_match_real_address_files() {
        let p = paths(&env());
        assert_eq!(
            p,
            vec![
                PathBuf::from("/home/u/.config/ibus/bus/abc-unix-1"),
                PathBuf::from("/home/u/.config/ibus/bus/abc-unix-wayland-0"),
            ]
        );
    }

    #[test]
    fn display_with_host_and_screen() {
        let mut e = env();
        e.display = Some("box:12.3".into());
        let p = paths(&e);
        assert!(p[0].ends_with("abc-box-12"));
    }

    #[test]
    fn display_without_colon_is_a_host() {
        let mut e = env();
        e.display = Some("weird".into());
        assert!(paths(&e)[0].ends_with("abc-weird-0"));
    }

    #[test]
    fn no_x11_display_means_only_the_wayland_file() {
        let mut e = env();
        e.display = None;
        assert_eq!(paths(&e).len(), 1);
    }

    #[test]
    fn wayland_display_is_reduced_to_its_file_name() {
        let mut e = env();
        e.wayland_display = Some("/run/user/1000/wayland-1".into());
        assert!(paths(&e).last().unwrap().ends_with("abc-unix-wayland-1"));
    }

    #[test]
    fn env_override_wins() {
        let mut e = env();
        e.ibus_address_file = Some("/tmp/x".into());
        assert_eq!(paths(&e), vec![PathBuf::from("/tmp/x")]);
    }

    #[test]
    fn contents_parse_back_and_strip_trailing_semicolon() {
        let text = contents("unix:path=/run/user/1000/bus;", 42, "ff");
        assert_eq!(
            text,
            "IBUS_ADDRESS=unix:path=/run/user/1000/bus,fcitx_random_string=ff\nIBUS_DAEMON_PID=42\n"
        );
        let (a, pid) = parse(&text).unwrap();
        assert_eq!(a, "unix:path=/run/user/1000/bus,fcitx_random_string=ff");
        assert_eq!(pid, 42);
    }

    #[test]
    fn a_real_stale_file_parses() {
        let text = "# This file is created by ibus-daemon, please do not modify it.\n\
                    IBUS_ADDRESS=unix:abstract=/home/alice/.cache/ibus/dbus-AbCdEfGh,guid=0123abcd\n\
                    IBUS_DAEMON_PID=3333\n";
        let (a, pid) = parse(text).unwrap();
        assert!(a.starts_with("unix:abstract="));
        assert_eq!(pid, 3333);
    }

    #[test]
    fn blank_file_is_not_an_address() {
        assert_eq!(parse(blank()), None);
    }

    #[test]
    fn publish_then_retract_leaves_a_blank_file() {
        let dir = tempfile::tempdir().unwrap();
        let paths = vec![dir.path().join("ibus/bus/a"), dir.path().join("ibus/bus/b")];
        let ours = publish(&paths, "unix:path=/run/user/1000/bus", 7).unwrap();
        assert!(still_ours(&paths, &ours));
        let on_disk = fs::read_to_string(&paths[0]).unwrap();
        assert!(on_disk.contains("IBUS_DAEMON_PID=7"));
        assert_eq!(
            fs::metadata(&paths[0]).unwrap().permissions().mode() & 0o777,
            0o600
        );
        retract(&paths, &ours);
        assert_eq!(parse(&fs::read_to_string(&paths[0]).unwrap()), None);
        assert!(!still_ours(&paths, &ours));
    }

    #[test]
    fn retract_leaves_someone_elses_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let paths = vec![dir.path().join("f")];
        let ours = publish(&paths, "unix:path=/x", 7).unwrap();
        write_atomic(
            &paths[0],
            "IBUS_ADDRESS=unix:path=/other\nIBUS_DAEMON_PID=9\n",
        )
        .unwrap();
        retract(&paths, &ours);
        assert!(fs::read_to_string(&paths[0]).unwrap().contains("/other"));
    }

    #[test]
    fn republishing_changes_the_address() {
        let dir = tempfile::tempdir().unwrap();
        let paths = vec![dir.path().join("f")];
        let a = publish(&paths, "unix:path=/x", 7).unwrap();
        let b = publish(&paths, "unix:path=/x", 7).unwrap();
        assert_ne!(a.address, b.address, "clients must see a changed address");
    }

    use std::os::unix::fs::PermissionsExt;
}
