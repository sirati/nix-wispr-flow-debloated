//! The call dictation uses: `org.wisprime.WisprIme1.Commit`.

use std::sync::Arc;

use tracing::{debug, info};
use zbus::interface;

use crate::ibus;
use crate::state::Shared;

/// Longest text accepted in one call.
pub const MAX_LEN: usize = 64 * 1024;

pub const BUS_NAME: &str = "org.wisprime.WisprIme";
pub const PATH: &str = "/org/wisprime/WisprIme";

pub struct WisprIme {
    pub shared: Arc<Shared>,
}

#[interface(name = "org.wisprime.WisprIme1")]
impl WisprIme {
    /// Commits `text` into the field that has keyboard focus, the way a typed
    /// character would arrive, and leaves the clipboard alone. Returns false,
    /// and does nothing, if no field that this input method serves has focus,
    /// so the caller can fall back to pasting. An empty `text` only asks
    /// whether there is such a field.
    async fn commit(&self, text: &str) -> bool {
        if text.len() > MAX_LEN {
            return false;
        }
        let Some(ic) = self.shared.focused() else {
            debug!(
                "commit of {} characters: no focused field",
                text.chars().count()
            );
            return false;
        };
        if !text.is_empty() {
            // Wait for any key being processed, so text and keys keep order.
            let _order = ic.gate.lock().await;
            ibus::commit_text(&self.shared.conn, &ic, text).await;
            // The count only: what was said is nobody's business but the field's.
            info!(
                "committed {} characters into input context {}",
                text.chars().count(),
                ic.id
            );
        }
        true
    }
}
