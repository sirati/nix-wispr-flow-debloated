//! Key names for the dictation shortcut, and the codes they stand for: evdev
//! codes (KEY_*, BTN_*) on Linux, virtual key codes (kVK_*) on macOS.
//!
//! A binding is written `[backend:][hold:|toggle:]Key[+Key...]`, e.g.
//! `RightAlt`, `toggle:RightAlt+Space` or `local:RightCommand`; the backend
//! (`wispr` or `local`) says where its dictations go. Names are matched
//! without regard to case.
//! Left and right modifiers are separate keys, as in Wispr's own shortcuts.
//! On Linux any evdev name works too (`KEY_F19`, `BTN_SIDE` or `BTN_TASK`
//! for a mouse button), `hid:DEVICE/REPORT:BYTE:BIT` is a button that only
//! shows in raw HID reports (hidraw.rs), and on both platforms `code:N` is
//! the raw code.

use super::chord::{Binding, Key, Mode};

/// Not on this platform.
const NONE: u16 = u16::MAX;

/// Where a binding's dictations can go (protocol.rs, Backend).
const BACKENDS: &[&str] = &["wispr", "local"];

/// Name, evdev code, macOS virtual key code.
const KEYS: &[(&str, u16, u16)] = &[
    ("A", 30, 0),
    ("B", 48, 11),
    ("C", 46, 8),
    ("D", 32, 2),
    ("E", 18, 14),
    ("F", 33, 3),
    ("G", 34, 5),
    ("H", 35, 4),
    ("I", 23, 34),
    ("J", 36, 38),
    ("K", 37, 40),
    ("L", 38, 37),
    ("M", 50, 46),
    ("N", 49, 45),
    ("O", 24, 31),
    ("P", 25, 35),
    ("Q", 16, 12),
    ("R", 19, 15),
    ("S", 31, 1),
    ("T", 20, 17),
    ("U", 22, 32),
    ("V", 47, 9),
    ("W", 17, 13),
    ("X", 45, 7),
    ("Y", 21, 16),
    ("Z", 44, 6),
    ("1", 2, 18),
    ("2", 3, 19),
    ("3", 4, 20),
    ("4", 5, 21),
    ("5", 6, 23),
    ("6", 7, 22),
    ("7", 8, 26),
    ("8", 9, 28),
    ("9", 10, 25),
    ("0", 11, 29),
    ("F1", 59, 122),
    ("F2", 60, 120),
    ("F3", 61, 99),
    ("F4", 62, 118),
    ("F5", 63, 96),
    ("F6", 64, 97),
    ("F7", 65, 98),
    ("F8", 66, 100),
    ("F9", 67, 101),
    ("F10", 68, 109),
    ("F11", 87, 103),
    ("F12", 88, 111),
    ("F13", 183, 105),
    ("F14", 184, 107),
    ("F15", 185, 113),
    ("F16", 186, 106),
    ("F17", 187, 64),
    ("F18", 188, 79),
    ("F19", 189, 80),
    ("F20", 190, 90),
    ("F21", 191, NONE),
    ("F22", 192, NONE),
    ("F23", 193, NONE),
    ("F24", 194, NONE),
    ("Space", 57, 49),
    ("Enter", 28, 36),
    ("Tab", 15, 48),
    ("Escape", 1, 53),
    ("Backspace", 14, 51),
    ("Delete", 111, 117),
    // The Mac's Help key sits where PC keyboards have Insert.
    ("Insert", 110, 114),
    ("Home", 102, 115),
    ("End", 107, 119),
    ("PageUp", 104, 116),
    ("PageDown", 109, 121),
    ("Up", 103, 126),
    ("Down", 108, 125),
    ("Left", 105, 123),
    ("Right", 106, 124),
    ("CapsLock", 58, 57),
    ("LeftShift", 42, 56),
    ("RightShift", 54, 60),
    ("LeftCtrl", 29, 59),
    ("RightCtrl", 97, 62),
    ("LeftAlt", 56, 58),
    ("RightAlt", 100, 61),
    ("LeftSuper", 125, 55),
    ("RightSuper", 126, 54),
    // Fn/Globe on a Mac. PC laptops mostly handle Fn in firmware and never
    // report it.
    ("Fn", 464, 63),
    ("Menu", 127, 110),
    ("Minus", 12, 27),
    ("Equal", 13, 24),
    ("LeftBracket", 26, 33),
    ("RightBracket", 27, 30),
    ("Backslash", 43, 42),
    ("Semicolon", 39, 41),
    ("Apostrophe", 40, 39),
    ("Grave", 41, 50),
    ("Comma", 51, 43),
    ("Period", 52, 47),
    ("Slash", 53, 44),
    ("PrintScreen", 99, NONE),
    ("ScrollLock", 70, NONE),
    ("Pause", 119, NONE),
    ("NumLock", 69, NONE),
    ("Mute", 113, 74),
    ("VolumeDown", 114, 73),
    ("VolumeUp", 115, 72),
    ("MicMute", 248, NONE),
];

/// Other names for the keys above: the Mac's, Wispr's and common spellings.
const ALIASES: &[(&str, &str)] = &[
    ("AltGr", "RightAlt"),
    ("LeftOption", "LeftAlt"),
    ("RightOption", "RightAlt"),
    ("LeftCommand", "LeftSuper"),
    ("RightCommand", "RightSuper"),
    ("LeftCmd", "LeftSuper"),
    ("RightCmd", "RightSuper"),
    ("LeftMeta", "LeftSuper"),
    ("RightMeta", "RightSuper"),
    ("LeftWin", "LeftSuper"),
    ("RightWin", "RightSuper"),
    ("LeftControl", "LeftCtrl"),
    ("RightControl", "RightCtrl"),
    ("Globe", "Fn"),
    ("Return", "Enter"),
    ("Esc", "Escape"),
    ("Del", "Delete"),
    ("Ins", "Insert"),
    ("PgUp", "PageUp"),
    ("PgDn", "PageDown"),
    ("Compose", "Menu"),
    ("Dot", "Period"),
    ("Quote", "Apostrophe"),
    ("Backquote", "Grave"),
];

/// Modifiers without a side, which a binding cannot use: say which one.
const UNSIDED: &[&str] = &[
    "Shift", "Ctrl", "Control", "Alt", "Option", "Super", "Command", "Cmd", "Meta", "Win",
];

/// The evdev and macOS codes of a named key.
fn lookup(name: &str) -> Option<(u16, u16)> {
    let name = ALIASES
        .iter()
        .find(|(alias, _)| alias.eq_ignore_ascii_case(name))
        .map_or(name, |(_, to)| *to);
    KEYS.iter()
        .find(|(n, _, _)| n.eq_ignore_ascii_case(name))
        .map(|&(_, evdev, mac)| (evdev, mac))
}

#[cfg(target_os = "macos")]
fn here((_, mac): (u16, u16)) -> u16 {
    mac
}

#[cfg(not(target_os = "macos"))]
fn here((evdev, _): (u16, u16)) -> u16 {
    evdev
}

/// The code of one key on this platform.
pub fn parse_key(name: &str) -> Result<Key, String> {
    let name = name.trim();
    if let Some(codes) = lookup(name) {
        return match here(codes) {
            NONE => Err(format!("{name}: no such key on this platform")),
            code => Ok(code),
        };
    }
    #[cfg(target_os = "linux")]
    if let Some(key) = super::hidraw::parse_key(name) {
        return key;
    }
    #[cfg(not(target_os = "linux"))]
    if name.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("hid:")) {
        return Err(format!("{name}: hid: buttons are only read on Linux"));
    }
    if let Some(n) = name.strip_prefix("code:") {
        return n.parse().map_err(|_| format!("{name}: not a key code"));
    }
    #[cfg(target_os = "linux")]
    if let Ok(code) = name.parse::<evdev::KeyCode>() {
        return Ok(code.code());
    }
    if UNSIDED.iter().any(|u| u.eq_ignore_ascii_case(name)) {
        return Err(format!("{name}: say Left{name} or Right{name}"));
    }
    Err(format!("{name}: unknown key"))
}

/// A name for `key`, for the shortcut test.
pub fn key_name(key: Key) -> String {
    if let Some((name, ..)) = KEYS.iter().find(|&&(_, e, m)| here((e, m)) == key) {
        return (*name).to_string();
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(name) = super::hidraw::key_name(key) {
            return name;
        }
        let name = format!("{:?}", evdev::KeyCode::new(key));
        if !name.starts_with("unknown") {
            return name;
        }
    }
    format!("code:{key}")
}

/// One binding: `[backend:][hold:|toggle:]Key[+Key...]`.
pub fn parse_binding(spec: &str) -> Result<Binding, String> {
    let mut mode = Mode::Hold;
    let mut backend = None;
    let mut keys = spec;
    while let Some((word, rest)) = keys.split_once(':') {
        match word {
            "hold" => mode = Mode::Hold,
            "toggle" => mode = Mode::Toggle,
            b if BACKENDS.contains(&b) => backend = Some(b.to_string()),
            // code:N
            _ => break,
        }
        keys = rest;
    }
    let mut codes = keys
        .split('+')
        .map(parse_key)
        .collect::<Result<Vec<_>, _>>()?;
    codes.sort_unstable();
    let count = codes.len();
    codes.dedup();
    if codes.len() != count {
        return Err(format!("{spec}: a key appears twice"));
    }
    Ok(Binding {
        keys: codes,
        mode,
        backend,
    })
}

/// All bindings; two may not use the same keys.
pub fn parse_bindings(specs: &[String]) -> Result<Vec<Binding>, String> {
    let bindings = specs
        .iter()
        .map(|s| parse_binding(s))
        .collect::<Result<Vec<_>, _>>()?;
    for (i, b) in bindings.iter().enumerate() {
        if bindings[..i].iter().any(|o| o.keys == b.keys) {
            return Err(format!("{}: bound twice", specs[i]));
        }
    }
    Ok(bindings)
}

// macOS reports modifiers as FlagsChanged events with the modifier state in
// the event's flags: one bit per side (the device-dependent NX_DEVICE*
// bits) and one per kind (kCGEventFlagMask*). Fn has only the latter.
// Tested on every platform.
#[cfg(any(target_os = "macos", test))]
const MAC_SHIFT: u64 = 0x2_0000;
#[cfg(any(target_os = "macos", test))]
const MAC_CONTROL: u64 = 0x4_0000;
#[cfg(any(target_os = "macos", test))]
const MAC_ALTERNATE: u64 = 0x8_0000;
#[cfg(any(target_os = "macos", test))]
const MAC_COMMAND: u64 = 0x10_0000;
#[cfg(any(target_os = "macos", test))]
const MAC_FN: u64 = 0x80_0000;

#[cfg(any(target_os = "macos", test))]
/// The macOS modifier keys: key code, its side's bit, its kind's bit, and
/// both sides' bits.
pub const MAC_MODIFIERS: &[(Key, u64, u64, u64)] = &[
    (59, 0x1, MAC_CONTROL, 0x2001),
    (62, 0x2000, MAC_CONTROL, 0x2001),
    (56, 0x2, MAC_SHIFT, 0x6),
    (60, 0x4, MAC_SHIFT, 0x6),
    (55, 0x8, MAC_COMMAND, 0x18),
    (54, 0x10, MAC_COMMAND, 0x18),
    (58, 0x20, MAC_ALTERNATE, 0x60),
    (61, 0x40, MAC_ALTERNATE, 0x60),
    (63, MAC_FN, MAC_FN, MAC_FN),
];

#[cfg(any(target_os = "macos", test))]
/// Whether the macOS modifier `key` is down according to an event's
/// `flags`; None if it is no modifier, or if the flags set its kind without
/// any side bit (some keyboards and synthetic events), which cannot tell.
pub fn mac_modifier_down(key: Key, flags: u64) -> Option<bool> {
    let &(_, side, kind, sides) = MAC_MODIFIERS.iter().find(|m| m.0 == key)?;
    if flags & side != 0 {
        Some(true)
    } else if flags & kind == 0 || flags & sides != 0 {
        Some(false)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_aliases_find_both_platforms_codes() {
        assert_eq!(lookup("RightAlt"), Some((100, 61)));
        assert_eq!(lookup("rightoption"), Some((100, 61)));
        assert_eq!(lookup("AltGr"), Some((100, 61)));
        assert_eq!(lookup("Fn"), Some((464, 63)));
        assert_eq!(lookup("Globe"), Some((464, 63)));
        assert_eq!(lookup("F19"), Some((189, 80)));
        assert_eq!(lookup("space"), Some((57, 49)));
        assert_eq!(lookup("RightCommand"), Some((126, 54)));
        assert_eq!(lookup("nonsense"), None);
    }

    #[test]
    fn no_two_names_share_a_code() {
        for (i, &(name, evdev, mac)) in KEYS.iter().enumerate() {
            for &(other, e, m) in &KEYS[..i] {
                assert_ne!(evdev, e, "{name} and {other} on Linux");
                assert!(mac == NONE || mac != m, "{name} and {other} on macOS");
                assert!(!name.eq_ignore_ascii_case(other), "{name} twice");
            }
        }
        for (alias, to) in ALIASES {
            assert!(lookup(to).is_some(), "{alias} points at nothing");
            assert!(
                !KEYS.iter().any(|(n, ..)| n.eq_ignore_ascii_case(alias)),
                "{alias} is also a key"
            );
        }
    }

    // Linux values from <linux/input-event-codes.h>, through the evdev crate.
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_codes_match_evdev_names() {
        for (name, evdev_name) in [
            ("A", "KEY_A"),
            ("Z", "KEY_Z"),
            ("0", "KEY_0"),
            ("F12", "KEY_F12"),
            ("F13", "KEY_F13"),
            ("F24", "KEY_F24"),
            ("RightAlt", "KEY_RIGHTALT"),
            ("LeftSuper", "KEY_LEFTMETA"),
            ("Fn", "KEY_FN"),
            ("Menu", "KEY_COMPOSE"),
            ("Insert", "KEY_INSERT"),
            ("MicMute", "KEY_MICMUTE"),
            ("Grave", "KEY_GRAVE"),
        ] {
            let code: evdev::KeyCode = evdev_name.parse().unwrap();
            assert_eq!(parse_key(name), Ok(code.code()), "{name}");
        }
        assert_eq!(parse_key("BTN_SIDE"), Ok(0x113));
        assert_eq!(key_name(0x113), "BTN_SIDE");
        // The Logitech mouse's spare button.
        assert_eq!(parse_key("BTN_TASK"), Ok(0x117));
        assert_eq!(key_name(0x117), "BTN_TASK");
        let b = parse_binding("wispr:BTN_TASK").unwrap();
        assert_eq!((b.keys, b.mode), (vec![0x117], Mode::Hold));
        assert_eq!(key_name(100), "RightAlt");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn hid_buttons_are_keys_of_their_own() {
        let b = parse_binding("wispr:toggle:hid:0b0e:0412/3:1:0").unwrap();
        assert_eq!((b.mode, b.backend.as_deref()), (Mode::Toggle, Some("wispr")));
        assert_eq!(b.keys.len(), 1);
        assert!(b.keys[0] >= super::super::hidraw::FIRST);
        assert_eq!(key_name(b.keys[0]), "hid:0b0e:0412/3:1:0");
        let b = parse_binding("local:hid:Jabra_SPEAK_410_USB/3:1:0/latch+RightAlt").unwrap();
        assert_eq!(b.keys.len(), 2);
        assert!(parse_binding("toggle:hid:0b0e:0412").is_err());
    }

    #[test]
    fn bindings_parse() {
        let b = parse_binding("toggle:RightAlt+Space").unwrap();
        assert_eq!(b.mode, Mode::Toggle);
        let mut keys = vec![parse_key("RightAlt").unwrap(), parse_key("Space").unwrap()];
        keys.sort_unstable();
        assert_eq!(b.keys, keys);
        assert_eq!(parse_binding("RightCtrl").unwrap().mode, Mode::Hold);
        assert_eq!(parse_binding("hold:code:500").unwrap().keys, vec![500]);
        assert_eq!(parse_binding("RightAlt").unwrap().backend, None);
        let b = parse_binding("local:toggle:RightAlt+Space").unwrap();
        assert_eq!((b.mode, b.backend.as_deref()), (Mode::Toggle, Some("local")));
        let b = parse_binding("hold:wispr:code:61").unwrap();
        assert_eq!((b.keys, b.backend.as_deref()), (vec![61], Some("wispr")));
        assert!(parse_binding("cloud:RightAlt").is_err());
        assert!(parse_binding("RightAlt+rightoption").is_err());
        assert!(parse_binding("Alt").unwrap_err().contains("LeftAlt or RightAlt"));
        assert!(parse_binding("Hyper").is_err());
        assert!(parse_binding("").is_err());
        let specs = |s: &[&str]| s.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(parse_bindings(&specs(&["RightAlt", "toggle:RightAlt+Space"])).is_ok());
        assert!(parse_bindings(&specs(&["RightAlt", "toggle:RightAlt"])).is_err());
    }

    #[test]
    fn mac_flags_tell_which_side_is_down() {
        let right_option = 61;
        let left_option = 58;
        // Right Option down: its side bit and the kind bit.
        assert_eq!(mac_modifier_down(right_option, MAC_ALTERNATE | 0x40), Some(true));
        assert_eq!(mac_modifier_down(left_option, MAC_ALTERNATE | 0x40), Some(false));
        // Both up.
        assert_eq!(mac_modifier_down(right_option, 0x100), Some(false));
        // The kind without a side cannot tell.
        assert_eq!(mac_modifier_down(right_option, MAC_ALTERNATE), None);
        // Fn has no sides.
        assert_eq!(mac_modifier_down(63, MAC_FN), Some(true));
        assert_eq!(mac_modifier_down(63, MAC_SHIFT | 0x2), Some(false));
        // Right Control's side bit is the odd one out.
        assert_eq!(mac_modifier_down(62, MAC_CONTROL | 0x2000), Some(true));
        assert_eq!(mac_modifier_down(59, MAC_CONTROL | 0x2000), Some(false));
        // Not a modifier.
        assert_eq!(mac_modifier_down(0, MAC_ALTERNATE | 0x40), None);
        // Every modifier's name points at its code.
        for &(key, ..) in MAC_MODIFIERS {
            assert!(KEYS.iter().any(|&(_, _, m)| m == key), "{key}");
        }
    }
}
