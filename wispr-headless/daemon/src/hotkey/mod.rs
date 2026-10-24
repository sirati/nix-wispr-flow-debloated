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
#[cfg(target_os = "linux")]
mod hidraw;
mod keys;
#[cfg(target_os = "macos")]
mod mac;

#[cfg(target_os = "linux")]
use evdev as platform;
#[cfg(target_os = "macos")]
use mac as platform;

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Instant;

pub use chord::{Binding, Chord, Hotkey, Key, ARM};
pub use keys::{key_name, parse_bindings};

/// Where the sources report what they see.
pub type Log = Arc<dyn Fn(String) + Send + Sync>;

/// Every key going down (true) or up (false), from all keyboards.
pub type KeySink = Sender<(Key, bool)>;

/// Watches the keyboards; `wanted` are the keys the bindings use (on Linux,
/// devices that have one of them are read even if they are no keyboard,
/// such as a mouse for its side buttons; and the /dev/hidraw nodes of
/// `hid:` buttons).
pub fn watch(wanted: &[Key], keys: KeySink, log: Log) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    hidraw::watch(wanted, keys.clone(), log.clone())?;
    platform::watch(wanted, keys, log)
}

/// Feeds the keys from `keys` to `chord`, and its timer, until the sources
/// end: `on_key` sees each key first, `on_event` what the chord makes of it
/// and the binding the dictation started on.
pub fn drive(
    mut chord: Chord,
    keys: Receiver<(Key, bool)>,
    mut on_key: impl FnMut(Key, bool),
    mut on_event: impl FnMut(Hotkey, &Binding),
) {
    loop {
        let next = match chord.deadline() {
            Some(at) => keys.recv_timeout(at.saturating_duration_since(Instant::now())),
            None => keys.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        let events = match next {
            Ok((key, down)) => {
                on_key(key, down);
                chord.key(key, down, Instant::now())
            }
            Err(RecvTimeoutError::Timeout) => chord.tick(Instant::now()),
            Err(RecvTimeoutError::Disconnected) => return,
        };
        for &e in events {
            if let Some(b) = chord.owner() {
                on_event(e, b);
            }
        }
    }
}

/// Watches the keyboards for `bindings`; `on_event` runs on a thread of its
/// own, in the order things happen, with the binding the dictation started
/// on.
pub fn spawn(
    bindings: Vec<Binding>,
    on_event: impl FnMut(Hotkey, &Binding) + Send + 'static,
    log: Log,
) -> Result<(), String> {
    let mut wanted: Vec<Key> = bindings.iter().flat_map(|b| b.keys.clone()).collect();
    wanted.sort_unstable();
    wanted.dedup();
    let (tx, rx) = mpsc::channel();
    watch(&wanted, tx, log)?;
    std::thread::Builder::new()
        .name("hotkey".into())
        .spawn(move || drive(Chord::new(bindings, ARM), rx, |_, _| {}, on_event))
        .map(|_| ())
        .map_err(|e| format!("cannot start the shortcut thread: {e}"))
}
