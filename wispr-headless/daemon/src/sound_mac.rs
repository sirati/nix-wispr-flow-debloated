//! Cached, asynchronous playback without a new afplay process for each cue.
//! All access stays on the daemon's serve thread, which owns these sounds.

use std::cell::RefCell;
use std::collections::HashMap;

use objc2::rc::{autoreleasepool, Retained};
use objc2::AnyThread;
use objc2_app_kit::NSSound;
use objc2_foundation::NSString;

thread_local! {
    static SOUNDS: RefCell<HashMap<String, Retained<NSSound>>> = RefCell::new(HashMap::new());
}

/// Explicit player arguments must keep their configured meaning.
pub fn uses_native(player: &[String]) -> bool {
    matches!(player, [program] if program == "/usr/bin/afplay" || program == "afplay")
}

/// Read the files into memory at startup. Return those needing the external
/// player as a fallback, for example an unsupported sound format.
pub fn prepare(paths: impl IntoIterator<Item = String>) -> Vec<String> {
    autoreleasepool(|_| {
        SOUNDS.with_borrow_mut(|sounds| {
            let mut failed = Vec::new();
            for path in paths {
                if sounds.contains_key(&path) {
                    continue;
                }
                match NSSound::initWithContentsOfFile_byReference(
                    NSSound::alloc(),
                    &NSString::from_str(&path),
                    false,
                ) {
                    Some(sound) => {
                        sounds.insert(path, sound);
                    }
                    None => failed.push(path),
                }
            }
            failed
        })
    })
}

pub fn play(path: &str) -> bool {
    autoreleasepool(|_| {
        SOUNDS.with_borrow(|sounds| {
            let Some(sound) = sounds.get(path) else {
                return false;
            };
            // NSSound otherwise ignores play() while this cue is playing.
            // A new recording's cue must still be heard if presses overlap.
            if sound.isPlaying() {
                sound.stop();
            }
            sound.play()
        })
    })
}
