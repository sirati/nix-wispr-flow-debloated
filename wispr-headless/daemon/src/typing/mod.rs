//! Types a transcript into the focused app, for a dictation this process
//! finishes itself (with Wispr, its helper types the text). `Typist` is the
//! whole interface: `type_text(&str)`.
//!
//! How, by `Method` (`--type-with`):
//!
//! - `auto`: GNOME with wispr-ime: the input method; other Linux desktops:
//!   keys; macOS: paste.
//! - `ime` (Linux): commit through wispr-ime (ime.rs), as the patched Wispr
//!   helper does. Only GNOME runs it; it answers false when no field it
//!   serves has focus (an XWayland window), and `auto` then types keys.
//! - `keys`: key presses. Linux, Wayland: a uinput keyboard (uinput.rs),
//!   which every compositor reads like a real one; X11: XTEST (x11.rs).
//!   Both look the characters up in the layout (keymap.rs). On X11 a
//!   character the layout lacks is typed by lending it a spare keycode for a
//!   moment; Wayland cannot, so there such a transcript is pasted whole if
//!   wl-copy is there. macOS: key events that carry their text
//!   (CGEventKeyboardSetUnicodeString), in any layout.
//! - `paste`: the clipboard and Ctrl+V (Wayland, through wl-copy) or Cmd+V
//!   (macOS, which puts back what the clipboard held). This is what Wispr
//!   does; editors then see one insertion instead of typed keys (no
//!   auto-closed brackets), but terminals want Ctrl+Shift+V.
//!
//! A new line is typed as Shift+Enter, so chat apps add a line instead of
//! sending; other control characters are left out (`pieces`).

#[cfg(target_os = "linux")]
mod dbus;
#[cfg(target_os = "linux")]
mod ime;
#[cfg(any(target_os = "linux", test))]
mod keymap;
#[cfg(any(target_os = "linux", test))]
mod keysyms;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "linux")]
mod uinput;
#[cfg(target_os = "linux")]
mod x11;

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
use mac as platform;

use std::time::Duration;

pub use crate::hotkey::Log;

#[cfg(target_os = "linux")]
/// The uinput keyboard's name, which hotkey/evdev.rs does not read.
pub const DEVICE_NAME: &str = "wispr-headless keyboard";

#[cfg(target_os = "macos")]
/// Marks the key events this process posts on macOS (their source user
/// data), so hotkey/mac.rs skips them: "WISPR" in ASCII.
pub const MAC_EVENT_MARK: i64 = 0x57_49_53_50_52;

/// Between two typed keys (or chunks of text on macOS), so slow apps keep
/// up; xdotool and ydotool wait 12 ms by default.
const KEY_GAP: Duration = Duration::from_millis(4);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Auto,
    Ime,
    Keys,
    Paste,
}

impl Method {
    pub fn parse(s: &str) -> Result<Method, String> {
        match s {
            "auto" => Ok(Method::Auto),
            "ime" => Ok(Method::Ime),
            "keys" => Ok(Method::Keys),
            "paste" => Ok(Method::Paste),
            _ => Err(format!("{s}: not auto, ime, keys or paste")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Options {
    pub method: Method,
    /// wl-copy, for pasting on Wayland.
    pub wl_copy: Option<String>,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            method: Method::Auto,
            wl_copy: None,
        }
    }
}

pub struct Typist(platform::Typist);

// A dictation may hand its typist to any thread.
const _: fn() = || {
    fn send<T: Send>() {}
    send::<Typist>();
};

impl Typist {
    /// Sets up what typing needs (on Wayland the uinput keyboard, which the
    /// compositor takes a moment to accept, so this is best done before the
    /// first transcript).
    pub fn new(options: Options, log: Log) -> Result<Typist, String> {
        platform::Typist::new(options, log).map(Typist)
    }

    /// Types `text` into the focused field. Blocks until done (a paste on
    /// macOS waits half a second to put the clipboard back).
    pub fn type_text(&mut self, text: &str) -> Result<(), String> {
        if text.is_empty() {
            return Ok(());
        }
        self.0.type_text(text)
    }
}

/// What a transcript is made of, for typing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Piece {
    Char(char),
    /// \n, \r\n, \r, or a Unicode line or paragraph separator.
    Newline,
    Tab,
}

/// `text` without control characters, new lines in one form.
pub fn pieces(text: &str) -> impl Iterator<Item = Piece> + '_ {
    let mut chars = text.chars().peekable();
    std::iter::from_fn(move || loop {
        let c = chars.next()?;
        return Some(match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                Piece::Newline
            }
            '\n' | '\u{2028}' | '\u{2029}' => Piece::Newline,
            '\t' => Piece::Tab,
            c if c.is_control() => continue,
            c => Piece::Char(c),
        });
    })
}

/// One key event on macOS (tested on every platform).
#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MacStep {
    /// UTF-16 text carried by one key event.
    Text(Vec<u16>),
    Newline,
}

#[cfg(any(target_os = "macos", test))]
/// Longest text one macOS key event carries reliably: the documented limit
/// of CGEventKeyboardSetUnicodeString.
pub const MAC_CHUNK: usize = 20;

#[cfg(any(target_os = "macos", test))]
/// `text` as macOS key events: up to `max` UTF-16 units each, never
/// splitting a surrogate pair.
pub fn mac_steps(text: &str, max: usize) -> Vec<MacStep> {
    let mut steps = Vec::new();
    let mut chunk: Vec<u16> = Vec::new();
    let mut units = [0u16; 2];
    for p in pieces(text) {
        let c = match p {
            Piece::Newline => {
                if !chunk.is_empty() {
                    steps.push(MacStep::Text(std::mem::take(&mut chunk)));
                }
                steps.push(MacStep::Newline);
                continue;
            }
            Piece::Tab => '\t',
            Piece::Char(c) => c,
        };
        let encoded = c.encode_utf16(&mut units);
        if chunk.len() + encoded.len() > max && !chunk.is_empty() {
            steps.push(MacStep::Text(std::mem::take(&mut chunk)));
        }
        chunk.extend_from_slice(encoded);
    }
    if !chunk.is_empty() {
        steps.push(MacStep::Text(chunk));
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pieces_drop_control_characters_and_unify_new_lines() {
        let p: Vec<Piece> = pieces("a\r\nb\rc\nd\u{2029}\t\u{7}e\u{200d}").collect();
        use Piece::*;
        assert_eq!(
            p,
            [
                Char('a'),
                Newline,
                Char('b'),
                Newline,
                Char('c'),
                Newline,
                Char('d'),
                Newline,
                Tab,
                Char('e'),
                // A joiner is part of the text, not a control character.
                Char('\u{200d}'),
            ]
        );
    }

    fn text(s: &str) -> MacStep {
        MacStep::Text(s.encode_utf16().collect())
    }

    #[test]
    fn mac_text_goes_in_chunks_that_keep_surrogate_pairs_whole() {
        assert_eq!(mac_steps("", MAC_CHUNK), []);
        assert_eq!(mac_steps("hello", MAC_CHUNK), [text("hello")]);
        assert_eq!(
            mac_steps("abcdefghij", 4),
            [text("abcd"), text("efgh"), text("ij")]
        );
        // 😀 is two units: it moves to the next chunk whole.
        assert_eq!(mac_steps("abc😀d", 4), [text("abc"), text("😀d")]);
        assert_eq!(
            mac_steps("one\r\ntwo\n\nthree\tfour", MAC_CHUNK),
            [
                text("one"),
                MacStep::Newline,
                text("two"),
                MacStep::Newline,
                MacStep::Newline,
                text("three\tfour"),
            ]
        );
        for step in mac_steps(&"Grüße, 😀 und € ".repeat(40), MAC_CHUNK) {
            let MacStep::Text(units) = step else { continue };
            assert!(units.len() <= MAC_CHUNK);
            assert!(String::from_utf16(&units).is_ok(), "a pair was split");
        }
    }
}
