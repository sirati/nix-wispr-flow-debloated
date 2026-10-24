//! Linux: the keys from /dev/input/event*, below the display server, so the
//! shortcut works the same on GNOME, KDE Plasma, any other Wayland compositor
//! and X11 (Wispr's own helper reads them the same way on Wayland). Devices
//! are only read, never grabbed: every key still reaches the desktop.
//!
//! Needs read access to the event nodes: the `input` group, which
//! programs.wisprFlow.users grants. Keyboards are read, and any other device
//! that has a key of the bindings (a mouse's side buttons, dictation-buttons'
//! virtual keyboard). Devices that appear later are picked up through
//! inotify on /dev/input, so a resting watcher has no timers. The keyboard
//! this process types with (typing/uinput.rs) is left out.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use evdev::{Device, EventSummary, KeyCode};
use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};

use super::{Key, KeySink, Log};

const DIR: &str = "/dev/input";

struct Watcher {
    wanted: Vec<Key>,
    keys: KeySink,
    log: Log,
    /// Event nodes being read.
    open: Mutex<HashSet<PathBuf>>,
}

pub fn watch(wanted: &[Key], keys: KeySink, log: Log) -> Result<(), String> {
    // Before the first scan, so no device can appear unnoticed in between.
    let inotify = Inotify::init(InitFlags::IN_CLOEXEC)
        .and_then(|i| {
            i.add_watch(DIR, AddWatchFlags::IN_CREATE | AddWatchFlags::IN_ATTRIB)
                .map(|_| i)
        })
        .map_err(|e| format!("cannot watch {DIR}: {e}"));
    let w = Arc::new(Watcher {
        wanted: wanted.to_vec(),
        keys,
        log,
        open: Mutex::new(HashSet::new()),
    });
    let mut denied = 0;
    let mut nodes: Vec<PathBuf> = std::fs::read_dir(DIR)
        .map_err(|e| format!("cannot list {DIR}: {e}"))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| is_event_node(p))
        .collect();
    nodes.sort();
    for path in nodes {
        if let Err(e) = w.clone().try_open(&path) {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                denied += 1;
            }
        }
    }
    let count = w.open.lock().expect("open lock poisoned").len();
    if count == 0 && denied > 0 {
        return Err(format!(
            "cannot read {DIR}: the user needs to be in the `input` group"
        ));
    }
    (w.log)(format!("shortcut: reading {count} input devices"));
    match inotify {
        Ok(inotify) => {
            std::thread::Builder::new()
                .name("hotkey-devices".into())
                .spawn(move || w.follow(inotify))
                .map_err(|e| format!("cannot start the device watcher: {e}"))?;
        }
        Err(e) => (w.log)(format!("shortcut: {e}; devices added later are not read")),
    }
    Ok(())
}

fn is_event_node(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("event"))
}

impl Watcher {
    /// New event nodes: udev creates them, then sets their group (IN_ATTRIB),
    /// so a node that cannot be opened yet is tried again then.
    fn follow(self: Arc<Self>, inotify: Inotify) {
        while let Ok(events) = inotify.read_events() {
            for e in events {
                let Some(name) = e.name else { continue };
                let path = Path::new(DIR).join(name);
                if is_event_node(&path) {
                    let _ = self.clone().try_open(&path);
                }
            }
        }
    }

    /// Starts reading `path` if it is a device we want and not read yet.
    fn try_open(self: Arc<Self>, path: &Path) -> std::io::Result<()> {
        if self.open.lock().expect("open lock poisoned").contains(path) {
            return Ok(());
        }
        let device = Device::open(path)?;
        if !self.wants(&device) {
            return Ok(());
        }
        if !self
            .open
            .lock()
            .expect("open lock poisoned")
            .insert(path.to_path_buf())
        {
            return Ok(());
        }
        let name = device.name().unwrap_or("?").to_string();
        (self.log)(format!("shortcut: reading {name}"));
        let path = path.to_path_buf();
        let spawned = std::thread::Builder::new()
            .name("hotkey-device".into())
            .spawn({
                let this = self.clone();
                let path = path.clone();
                move || this.read(&path, device)
            });
        if spawned.is_err() {
            self.open.lock().expect("open lock poisoned").remove(&path);
        }
        Ok(())
    }

    fn wants(&self, device: &Device) -> bool {
        if device.name() == Some(crate::typing::DEVICE_NAME) {
            return false;
        }
        let Some(keys) = device.supported_keys() else {
            return false;
        };
        let keyboard = keys.contains(KeyCode::KEY_A) && keys.contains(KeyCode::KEY_Z);
        keyboard || self.wanted.iter().any(|&k| keys.contains(KeyCode::new(k)))
    }

    /// Reads one device until it goes away. evdev's sync after a dropped
    /// buffer (SYN_DROPPED) reports the keys that changed in the gap.
    fn read(&self, path: &Path, mut device: Device) {
        let mut down: Vec<Key> = Vec::new();
        'read: loop {
            let events = match device.fetch_events() {
                Ok(events) => events,
                Err(_) => break,
            };
            for e in events {
                let EventSummary::Key(_, code, value) = e.destructure() else {
                    continue;
                };
                let key = code.code();
                // 2 is auto-repeat.
                let pressed = match value {
                    1 => true,
                    0 => false,
                    _ => continue,
                };
                if pressed {
                    down.push(key);
                } else {
                    down.retain(|&k| k != key);
                }
                if self.keys.send((key, pressed)).is_err() {
                    break 'read;
                }
            }
        }
        // Unplugged: whatever it held is up now.
        for key in down {
            let _ = self.keys.send((key, false));
        }
        self.open.lock().expect("open lock poisoned").remove(path);
        (self.log)(format!(
            "shortcut: stopped reading {}",
            device.name().unwrap_or("?")
        ));
    }
}
