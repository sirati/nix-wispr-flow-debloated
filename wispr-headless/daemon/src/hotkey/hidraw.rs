//! Linux: buttons that never become key events, only raw HID reports, such
//! as a speakerphone's call button (the Jabra SPEAK 410's is a Telephony
//! usage that the kernel's hid-jabra driver drops). A binding names one as
//! a key, `hid:DEVICE/REPORT:BYTE:BIT` (keys.rs), which stands for one bit of
//! one report of a device read from /dev/hidraw*:
//!
//! - DEVICE is `vendor:product` in hex (`0b0e:0412`), or the device's HID
//!   name with `_` for spaces (`Jabra_SPEAK_410_USB`), matched without
//!   regard to case.
//! - REPORT is the report's first byte, its id on devices with numbered
//!   reports; BYTE counts from the byte after it (1); BIT is 0 for the
//!   lowest.
//! - The bit going to 1 is the key going down, back to 0 up. A button that
//!   holds its state until pressed again (a latching switch) is written
//!   with `/latch`: each change of the bit is one press and release.
//!
//! Each such key gets a code of its own from `FIRST` on, above evdev's
//! KEY_MAX, so they mix with the real keys in chord.rs. Devices are only
//! read; the nodes need read access (the `input` group through a udev rule,
//! which programs.wisprFlow adds for `vendor:product` buttons). Devices that
//! appear later are picked up through inotify on /dev, as evdev.rs does for
//! /dev/input. Nothing is logged per report.

use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};

use super::{Key, KeySink, Log};

/// The first code for a HID button: evdev's codes end at KEY_MAX, 0x2ff.
pub const FIRST: Key = 0x300;
/// Room for this many buttons.
const COUNT: usize = 0x100;

const DIR: &str = "/dev";
const SYS: &str = "/sys/class/hidraw";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Device {
    Id {
        vendor: u16,
        product: u16,
    },
    /// Lower case, `_` for spaces.
    Name(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Button {
    /// As written after `hid:`, for its name.
    spec: String,
    device: Device,
    report: u8,
    /// 1 is the byte after the report's first.
    byte: usize,
    bit: u8,
    latch: bool,
}

/// The buttons the bindings named; button i is key FIRST + i. Filled as the
/// bindings are parsed, so the same spec always gets the same code.
static BUTTONS: Mutex<Vec<Button>> = Mutex::new(Vec::new());

fn buttons() -> std::sync::MutexGuard<'static, Vec<Button>> {
    BUTTONS.lock().unwrap_or_else(|e| e.into_inner())
}

/// The key a `hid:` name stands for; None if `name` is no `hid:` name.
pub fn parse_key(name: &str) -> Option<Result<Key, String>> {
    let spec = name
        .get(..4)
        .filter(|p| p.eq_ignore_ascii_case("hid:"))
        .map(|_| &name[4..])?;
    Some(parse(spec).and_then(|button| {
        let mut all = buttons();
        let index = match all.iter().position(|b| *b == button) {
            Some(i) => i,
            None if all.len() < COUNT => {
                all.push(button);
                all.len() - 1
            }
            None => return Err(format!("{name}: too many hid: buttons")),
        };
        Ok(FIRST + index as Key)
    }))
}

/// The name of a `hid:` key, for the shortcut test.
pub fn key_name(key: Key) -> Option<String> {
    let index = usize::from(key.checked_sub(FIRST)?);
    buttons().get(index).map(|b| format!("hid:{}", b.spec))
}

fn parse(spec: &str) -> Result<Button, String> {
    let bad = |why: &str| format!("hid:{spec}: {why}; write hid:DEVICE/REPORT:BYTE:BIT[/latch]");
    let mut parts = spec.split('/');
    let device = parts.next().unwrap_or_default();
    let place = parts.next().ok_or_else(|| bad("no report"))?;
    let latch = match parts.next() {
        None => false,
        Some(l) if l.eq_ignore_ascii_case("latch") => true,
        Some(_) => return Err(bad("only /latch can follow the bit")),
    };
    if parts.next().is_some() {
        return Err(bad("too many parts"));
    }
    let device = parse_device(device).ok_or_else(|| bad("no device"))?;
    let numbers: Vec<&str> = place.split(':').collect();
    let [report, byte, bit] = numbers[..] else {
        return Err(bad("the report is REPORT:BYTE:BIT"));
    };
    let report = report.parse().map_err(|_| bad("REPORT is 0 to 255"))?;
    let byte = byte
        .parse()
        .ok()
        .filter(|b| (1..4096).contains(b))
        .ok_or_else(|| bad("BYTE counts from 1"))?;
    let bit = bit
        .parse()
        .ok()
        .filter(|&b| b < 8)
        .ok_or_else(|| bad("BIT is 0 to 7"))?;
    Ok(Button {
        spec: spec.to_string(),
        device,
        report,
        byte,
        bit,
        latch,
    })
}

fn parse_device(device: &str) -> Option<Device> {
    if device.is_empty() {
        return None;
    }
    let hex = |s: &str| {
        (s.len() == 4)
            .then(|| u16::from_str_radix(s, 16).ok())
            .flatten()
    };
    if let Some((v, p)) = device.split_once(':') {
        if let (Some(vendor), Some(product)) = (hex(v), hex(p)) {
            return Some(Device::Id { vendor, product });
        }
    }
    Some(Device::Name(name_key(device)))
}

/// How names compare: without case, `_` for spaces.
fn name_key(name: &str) -> String {
    name.trim().replace(' ', "_").to_lowercase()
}

/// What /sys tells about a hidraw node's device: from its uevent's HID_ID
/// (`bus:vendor:product`, 8 hex digits each) and HID_NAME.
#[derive(Debug, Default, PartialEq, Eq)]
struct Info {
    vendor: u16,
    product: u16,
    name: String,
}

fn parse_uevent(uevent: &str) -> Info {
    let mut info = Info::default();
    for line in uevent.lines() {
        if let Some(id) = line.strip_prefix("HID_ID=") {
            let mut ids = id
                .split(':')
                .skip(1)
                .map(|n| u32::from_str_radix(n, 16).ok());
            if let (Some(Some(v)), Some(Some(p))) = (ids.next(), ids.next()) {
                // Bluetooth and USB ids are 16 bits wide.
                info.vendor = v as u16;
                info.product = p as u16;
            }
        } else if let Some(name) = line.strip_prefix("HID_NAME=") {
            info.name = name.to_string();
        }
    }
    info
}

impl Device {
    fn matches(&self, info: &Info) -> bool {
        match self {
            Device::Id { vendor, product } => (info.vendor, info.product) == (*vendor, *product),
            Device::Name(name) => *name == name_key(&info.name),
        }
    }
}

/// The keys one report changes, given each button's last state (`levels`,
/// as many as `buttons`): pushed onto `out`.
fn report_keys(
    buttons: &[(Key, Button)],
    levels: &mut [bool],
    data: &[u8],
    out: &mut Vec<(Key, bool)>,
) {
    let Some(&report) = data.first() else { return };
    for ((key, b), level) in buttons.iter().zip(levels.iter_mut()) {
        if b.report != report {
            continue;
        }
        let Some(&byte) = data.get(b.byte) else {
            continue;
        };
        let now = byte >> b.bit & 1 == 1;
        if now == *level {
            continue;
        }
        *level = now;
        if b.latch {
            out.push((*key, true));
            out.push((*key, false));
        } else {
            out.push((*key, now));
        }
    }
}

struct Watcher {
    /// The buttons the bindings use, with their keys.
    buttons: Vec<(Key, Button)>,
    keys: KeySink,
    log: Log,
    /// Nodes being read.
    open: Mutex<HashSet<PathBuf>>,
    /// Nodes of a wanted device that could not be opened, said once each.
    denied: Mutex<HashSet<PathBuf>>,
}

/// Reads the hidraw devices of the `hid:` keys among `wanted`; nothing to do
/// without any.
pub fn watch(wanted: &[Key], keys: KeySink, log: Log) -> Result<(), String> {
    let buttons: Vec<(Key, Button)> = buttons()
        .iter()
        .enumerate()
        .map(|(i, b)| (FIRST + i as Key, b.clone()))
        .filter(|(k, _)| wanted.contains(k))
        .collect();
    if buttons.is_empty() {
        return Ok(());
    }
    // Before the first scan, so no device can appear unnoticed in between.
    let inotify = Inotify::init(InitFlags::IN_CLOEXEC).and_then(|i| {
        i.add_watch(DIR, AddWatchFlags::IN_CREATE | AddWatchFlags::IN_ATTRIB)
            .map(|_| i)
    });
    let w = Arc::new(Watcher {
        buttons,
        keys,
        log,
        open: Mutex::new(HashSet::new()),
        denied: Mutex::new(HashSet::new()),
    });
    let mut nodes: Vec<String> = std::fs::read_dir(SYS)
        .map(|d| {
            d.flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    nodes.sort();
    for node in &nodes {
        w.clone().try_open(node);
    }
    match inotify {
        Ok(inotify) => {
            std::thread::Builder::new()
                .name("hotkey-hidraw".into())
                .spawn(move || w.follow(inotify))
                .map_err(|e| format!("cannot start the hidraw watcher: {e}"))?;
        }
        Err(e) => (w.log)(format!(
            "shortcut: cannot watch {DIR}: {e}; HID devices added later are not read"
        )),
    }
    Ok(())
}

impl Watcher {
    /// New nodes: the kernel creates them, then udev sets their group
    /// (IN_ATTRIB), so a node that cannot be opened yet is tried again then.
    fn follow(self: Arc<Self>, inotify: Inotify) {
        while let Ok(events) = inotify.read_events() {
            for e in events {
                let Some(name) = e.name.as_ref().and_then(|n| n.to_str()) else {
                    continue;
                };
                if name.starts_with("hidraw") {
                    self.clone().try_open(name);
                }
            }
        }
    }

    /// Starts reading /dev/`node` if it is a device of a button and not read
    /// yet.
    fn try_open(self: Arc<Self>, node: &str) {
        let path = Path::new(DIR).join(node);
        if self
            .open
            .lock()
            .expect("open lock poisoned")
            .contains(&path)
        {
            return;
        }
        let Ok(uevent) = std::fs::read_to_string(format!("{SYS}/{node}/device/uevent")) else {
            return;
        };
        let info = parse_uevent(&uevent);
        let buttons: Vec<(Key, Button)> = self
            .buttons
            .iter()
            .filter(|(_, b)| b.device.matches(&info))
            .cloned()
            .collect();
        if buttons.is_empty() {
            return;
        }
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(e) => {
                if self
                    .denied
                    .lock()
                    .expect("denied lock poisoned")
                    .insert(path.clone())
                {
                    (self.log)(format!(
                        "shortcut: cannot read {} ({}): {e}",
                        path.display(),
                        info.name
                    ));
                }
                return;
            }
        };
        if !self
            .open
            .lock()
            .expect("open lock poisoned")
            .insert(path.clone())
        {
            return;
        }
        self.denied
            .lock()
            .expect("denied lock poisoned")
            .remove(&path);
        (self.log)(format!("shortcut: reading {} (hidraw)", info.name));
        let spawned = std::thread::Builder::new()
            .name("hotkey-hid".into())
            .spawn({
                let this = self.clone();
                let path = path.clone();
                move || this.read(&path, file, &buttons, &info.name)
            });
        if spawned.is_err() {
            self.open.lock().expect("open lock poisoned").remove(&path);
        }
    }

    /// Reads one device until it goes away; hidraw gives one report per
    /// read.
    fn read(&self, path: &Path, mut file: File, buttons: &[(Key, Button)], name: &str) {
        let mut levels = vec![false; buttons.len()];
        let mut buf = [0u8; 4096];
        let mut out = Vec::new();
        'read: loop {
            let n = match file.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            out.clear();
            report_keys(buttons, &mut levels, &buf[..n], &mut out);
            for &event in &out {
                if self.keys.send(event).is_err() {
                    break 'read;
                }
            }
        }
        // Unplugged: whatever it held is up now.
        for ((key, _), down) in buttons.iter().zip(levels) {
            if down {
                let _ = self.keys.send((*key, false));
            }
        }
        self.open.lock().expect("open lock poisoned").remove(path);
        (self.log)(format!("shortcut: stopped reading {name} (hidraw)"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_parse() {
        let b = parse("0b0e:0412/3:1:0").unwrap();
        assert_eq!(
            b.device,
            Device::Id {
                vendor: 0x0b0e,
                product: 0x0412
            }
        );
        assert_eq!((b.report, b.byte, b.bit, b.latch), (3, 1, 0, false));
        let b = parse("Jabra_SPEAK_410_USB/1:2:7/latch").unwrap();
        assert_eq!(b.device, Device::Name("jabra_speak_410_usb".into()));
        assert_eq!((b.report, b.byte, b.bit, b.latch), (1, 2, 7, true));
        // Not hex, so a name.
        assert_eq!(
            parse("0b0e:041g/3:1:0").unwrap().device,
            Device::Name("0b0e:041g".into())
        );
        for bad in [
            "",
            "0b0e:0412",
            "/3:1:0",
            "0b0e:0412/3:1",
            "0b0e:0412/3:0:0",
            "0b0e:0412/3:1:8",
            "0b0e:0412/256:1:0",
            "0b0e:0412/3:1:0/hold",
            "0b0e:0412/3:1:0/latch/x",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn keys_are_numbered_once_per_spec() {
        let a = parse_key("hid:0b0e:0412/3:1:0").unwrap().unwrap();
        let again = parse_key("HID:0b0e:0412/3:1:0").unwrap().unwrap();
        let b = parse_key("hid:0b0e:0412/3:1:1").unwrap().unwrap();
        assert_eq!(a, again);
        assert_ne!(a, b);
        assert!(a >= FIRST && b >= FIRST);
        assert_eq!(key_name(a).as_deref(), Some("hid:0b0e:0412/3:1:0"));
        assert_eq!(key_name(FIRST + COUNT as Key - 1), None);
        assert_eq!(key_name(0x2ff), None);
        assert!(parse_key("BTN_TASK").is_none());
        assert!(parse_key("hid:nothing").unwrap().is_err());
    }

    #[test]
    fn uevents_say_vendor_product_and_name() {
        let info = parse_uevent(
            "DRIVER=hid-jabra\nHID_ID=0003:00000B0E:00000412\nHID_NAME=Jabra SPEAK 410 USB\n\
             HID_PHYS=usb-0000:00:14.0-1/input3\n",
        );
        assert_eq!(
            info,
            Info {
                vendor: 0x0b0e,
                product: 0x0412,
                name: "Jabra SPEAK 410 USB".into()
            }
        );
        assert!(parse("0b0e:0412/3:1:0").unwrap().device.matches(&info));
        assert!(parse("jabra_speak_410_usb/3:1:0")
            .unwrap()
            .device
            .matches(&info));
        assert!(!parse("0b0e:0413/3:1:0").unwrap().device.matches(&info));
        assert!(!parse("Jabra/3:1:0").unwrap().device.matches(&info));
    }

    #[test]
    fn reports_press_and_release() {
        let call = (FIRST, parse("0b0e:0412/3:1:0").unwrap());
        let latch = (FIRST + 1, parse("0b0e:0412/3:2:4/latch").unwrap());
        let buttons = [call, latch];
        let mut levels = [false, false];
        let mut out = Vec::new();
        let mut feed = |data: &[u8]| {
            out.clear();
            report_keys(&buttons, &mut levels, data, &mut out);
            out.clone()
        };
        // The Jabra's hook switch: a 1, then a 0.
        assert_eq!(feed(&[3, 0x01, 0]), [(FIRST, true)]);
        // The same state again, or another report, changes nothing.
        assert_eq!(feed(&[3, 0x01, 0]), []);
        assert_eq!(feed(&[1, 0x00, 0]), []);
        assert_eq!(feed(&[3, 0x00, 0]), [(FIRST, false)]);
        // A latching bit: each change is a press.
        assert_eq!(feed(&[3, 0, 0x10]), [(FIRST + 1, true), (FIRST + 1, false)]);
        assert_eq!(feed(&[3, 0, 0x00]), [(FIRST + 1, true), (FIRST + 1, false)]);
        // Too short for the byte: ignored.
        assert_eq!(feed(&[3]), []);
        assert_eq!(feed(&[]), []);
    }
}
