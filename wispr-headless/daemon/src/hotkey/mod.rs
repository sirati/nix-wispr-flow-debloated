//! The dictation shortcut, watched by this process itself rather than through
//! Wispr's helper: Linux reads /dev/input (evdev.rs), macOS taps the
//! session's key events (mac.rs). chord.rs turns the keys into `Hotkey`
//! events; keys.rs names the keys.
//!
//! Nothing per key is logged or kept beyond the keys held right now, except
//! in the shortcut test (`--hotkey-test`), which prints each key's name.

mod chord;
#[cfg(target_os = "linux")]
mod evdev;
mod keys;
#[cfg(target_os = "macos")]
mod mac;

#[cfg(target_os = "linux")]
use evdev as platform;
#[cfg(target_os = "macos")]
use mac as platform;

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub use chord::{Binding, Chord, Hotkey, Key, ARM, DOUBLE_PRESS, RESET};
#[cfg(test)]
pub use chord::Mode;
pub use keys::{key_name, parse_bindings, parse_key};

/// Where the sources report what they see.
pub type Log = Arc<dyn Fn(String) + Send + Sync>;

/// Every key going down (true) or up (false), from all keyboards.
pub type KeySink = Sender<(Key, bool)>;

/// Watches the keyboards; `wanted` are the keys the bindings use (on Linux,
/// devices that have one of them are read even if they are no keyboard,
/// such as a mouse for its side buttons).
pub fn watch(wanted: &[Key], keys: KeySink, log: Log) -> Result<(), String> {
    platform::watch(wanted, keys, log)
}

/// Feeds the keys from `keys` to `chord`, and its timer, until the sources
/// end: `on_key` sees each key first, `on_events` what the chord makes of it
/// (all events of one key or tick together) and the binding the dictation
/// started on.
pub fn drive(
    mut chord: Chord,
    keys: Receiver<(Key, bool)>,
    mut on_key: impl FnMut(Key, bool),
    mut on_events: impl FnMut(&[Hotkey], &Binding),
) {
    loop {
        let next = match chord.deadline() {
            Some(at) => keys.recv_timeout(at.saturating_duration_since(Instant::now())),
            None => keys.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        let events = match next {
            Ok((key, down)) => {
                if key != RESET {
                    on_key(key, down);
                }
                chord.key(key, down, Instant::now())
            }
            Err(RecvTimeoutError::Timeout) => chord.tick(Instant::now()),
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if events.is_empty() {
            continue;
        }
        if let Some(b) = chord.owner() {
            on_events(&events, b);
        }
    }
}

/// How the shortcut thread is told things besides keys.
#[derive(Clone)]
pub struct Control(KeySink);

impl Control {
    /// The dictation ended without the keys (a time limit, an error, the
    /// bar): the shortcut stops waiting for the press that would end it.
    pub fn reset(&self) {
        let _ = self.0.send((RESET, true));
    }

    /// Tests (`--hotkey-source stdin`): a key as if from a keyboard.
    pub fn key(&self, key: Key, down: bool) {
        if key != RESET {
            let _ = self.0.send((key, down));
        }
    }
}

/// Where the keys come from.
pub enum Source {
    /// The keyboards (evdev, the macOS event tap).
    Keyboards,
    /// Only `Control::key` (tests).
    Fed,
}

/// Watches the keyboards for `bindings`; `on_events` runs on a thread of its
/// own, in the order things happen, with the binding the dictation started
/// on. Two presses of a hold binding within `double` make a hands-free
/// dictation; Escape cancels one.
pub fn spawn(
    bindings: Vec<Binding>,
    double: Duration,
    source: Source,
    on_events: impl FnMut(&[Hotkey], &Binding) + Send + 'static,
    log: Log,
) -> Result<Control, String> {
    let escape = keys::parse_key("Escape").ok();
    let mut wanted: Vec<Key> = bindings.iter().flat_map(|b| b.keys.clone()).collect();
    wanted.extend(escape);
    wanted.sort_unstable();
    wanted.dedup();
    let (tx, rx) = mpsc::channel();
    if let Source::Keyboards = source {
        watch(&wanted, tx.clone(), log)?;
    }
    let chord = Chord::new(bindings, ARM).double_press(double).escape(escape);
    std::thread::Builder::new()
        .name("hotkey".into())
        .spawn(move || drive(chord, rx, |_, _| {}, on_events))
        .map(|_| Control(tx))
        .map_err(|e| format!("cannot start the shortcut thread: {e}"))
}

/// The code Wispr's helper reports for `key`, as in Wispr's shortcut
/// settings: the same macOS virtual key code on the Mac; on Linux, where
/// Wispr runs its Windows build, the Windows virtual-key code.
#[cfg(target_os = "macos")]
pub fn wispr_code(key: Key) -> Option<u16> {
    (key != RESET).then_some(key)
}

/// The code Wispr's helper reports for `key`, as in Wispr's shortcut
/// settings: the same macOS virtual key code on the Mac; on Linux, where
/// Wispr runs its Windows build, the Windows virtual-key code.
#[cfg(not(target_os = "macos"))]
pub fn wispr_code(key: Key) -> Option<u16> {
    // evdev code, Windows virtual-key code.
    const VK: &[(u16, u16)] = &[
        (1, 0x1b),   // Escape
        (29, 0xa2),  // Left Ctrl
        (97, 0xa3),  // Right Ctrl
        (42, 0xa0),  // Left Shift
        (54, 0xa1),  // Right Shift
        (56, 0xa4),  // Left Alt
        (100, 0xa5), // Right Alt
        (125, 0x5b), // Left Super
        (126, 0x5c), // Right Super
        (57, 0x20),  // Space
    ];
    if let Some(&(_, vk)) = VK.iter().find(|&&(e, _)| e == key) {
        return Some(vk);
    }
    match key {
        // F13..F24
        183..=194 => Some(key - 183 + 0x7c),
        _ => None,
    }
}
