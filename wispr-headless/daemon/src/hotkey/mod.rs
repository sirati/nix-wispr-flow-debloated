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

use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::time::Instant;

pub use chord::{Binding, Chord, Hotkey, Key, CANCEL_WINDOW};
pub use keys::{key_name, parse_bindings};

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

/// Watches the keyboards for `bindings`; `on_event` runs on a thread of its
/// own, in the order things happen.
pub fn spawn(
    bindings: Vec<Binding>,
    mut on_event: impl FnMut(Hotkey) + Send + 'static,
    log: Log,
) -> Result<(), String> {
    let mut wanted: Vec<Key> = bindings.iter().flat_map(|b| b.keys.clone()).collect();
    wanted.sort_unstable();
    wanted.dedup();
    let (tx, rx) = mpsc::channel();
    watch(&wanted, tx, log)?;
    std::thread::Builder::new()
        .name("hotkey".into())
        .spawn(move || {
            let mut chord = chord::Chord::new(bindings, CANCEL_WINDOW);
            for (key, down) in rx {
                for &e in chord.key(key, down, Instant::now()) {
                    on_event(e);
                }
            }
        })
        .map(|_| ())
        .map_err(|e| format!("cannot start the shortcut thread: {e}"))
}
