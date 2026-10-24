//! GNOME: commits text through wispr-ime (org.wisprime.WisprIme1.Commit on
//! the session bus), the call the patched Wispr helper makes too
//! (wispr-ime/helper-ime-commit.patch). The text arrives like typed text,
//! whatever the layout, and the clipboard is left alone.

use std::time::Duration;

use super::dbus::{self, Call};

const COMMIT: Call = Call {
    destination: "org.wisprime.WisprIme",
    path: "/org/wisprime/WisprIme",
    interface: "org.wisprime.WisprIme1",
    member: "Commit",
};

/// Longer than a healthy call; short enough that a hung service does not
/// hold up the transcript.
const TIMEOUT: Duration = Duration::from_millis(1500);

/// True if wispr-ime committed `text`; false if no field it serves has
/// focus; an error if it is not running (any desktop but GNOME) or hangs.
pub fn commit(text: &str) -> Result<bool, String> {
    dbus::call_bool(&COMMIT, text, TIMEOUT).map_err(|e| format!("wispr-ime: {e}"))
}
