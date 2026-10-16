//! State shared by the IBus interfaces, the fcitx5 signal router and the
//! injection call.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use zbus::zvariant::OwnedObjectPath;
use zbus::Connection;

use crate::wire;

/// One input context that an IBus client (mutter, usually) created, paired
/// with the input context we created for it in fcitx5.
pub struct Ic {
    pub id: u32,
    /// Unique bus name of the IBus client; only it may use this context.
    pub client: String,
    pub ibus_path: OwnedObjectPath,
    pub fcitx_path: String,
    /// Keeps calls for this context in the order they arrived, even though
    /// the D-Bus server handles each call in its own task.
    pub gate: tokio::sync::Mutex<()>,
    pub focused: AtomicBool,
    pub focus_stamp: AtomicU64,
    pub client_commit_preedit: AtomicBool,
    pub relative_rect: AtomicBool,
    pub ibus_caps: AtomicU32,
    pub content_flags: AtomicU64,
    /// keysym -> evdev keycode of keys recently sent to fcitx5. fcitx5's
    /// `ForwardKey` signal carries no keycode but IBus's does.
    pub keycodes: Mutex<HashMap<u32, u32>>,
}

impl Ic {
    pub fn new(id: u32, client: String, ibus_path: OwnedObjectPath, fcitx_path: String) -> Ic {
        Ic {
            id,
            client,
            ibus_path,
            fcitx_path,
            gate: tokio::sync::Mutex::new(()),
            focused: AtomicBool::new(false),
            focus_stamp: AtomicU64::new(0),
            client_commit_preedit: AtomicBool::new(false),
            relative_rect: AtomicBool::new(false),
            ibus_caps: AtomicU32::new(0),
            content_flags: AtomicU64::new(0),
            keycodes: Mutex::new(HashMap::new()),
        }
    }

    /// The capability word to give fcitx5: what the client can do, which
    /// kind of field it is, and how it reports the cursor.
    pub fn fcitx_caps(&self) -> u64 {
        wire::capabilities(
            self.ibus_caps.load(Ordering::Relaxed),
            self.relative_rect.load(Ordering::Relaxed),
        ) | self.content_flags.load(Ordering::Relaxed)
    }

    pub fn remember_keycode(&self, keysym: u32, keycode: u32) {
        let mut map = self.keycodes.lock().expect("keycode map poisoned");
        if map.len() > 512 {
            map.clear();
        }
        map.insert(keysym, keycode);
    }

    pub fn keycode_for(&self, keysym: u32) -> u32 {
        self.keycodes
            .lock()
            .expect("keycode map poisoned")
            .get(&keysym)
            .copied()
            .unwrap_or(0)
    }
}

pub struct Shared {
    pub conn: Connection,
    next_id: AtomicU32,
    focus_clock: AtomicU64,
    by_id: Mutex<HashMap<u32, Arc<Ic>>>,
}

impl Shared {
    pub fn new(conn: Connection) -> Arc<Shared> {
        Arc::new(Shared {
            conn,
            next_id: AtomicU32::new(0),
            focus_clock: AtomicU64::new(1),
            by_id: Mutex::new(HashMap::new()),
        })
    }

    pub fn allocate_id(&self) -> u32 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn insert(&self, ic: Arc<Ic>) {
        self.by_id
            .lock()
            .expect("ic map poisoned")
            .insert(ic.id, ic);
    }

    pub fn remove(&self, id: u32) -> Option<Arc<Ic>> {
        self.by_id.lock().expect("ic map poisoned").remove(&id)
    }

    pub fn all(&self) -> Vec<Arc<Ic>> {
        self.by_id
            .lock()
            .expect("ic map poisoned")
            .values()
            .cloned()
            .collect()
    }

    pub fn by_fcitx_path(&self, path: &str) -> Option<Arc<Ic>> {
        self.all().into_iter().find(|ic| ic.fcitx_path == path)
    }

    pub fn of_client(&self, client: &str) -> Vec<Arc<Ic>> {
        self.all()
            .into_iter()
            .filter(|ic| ic.client == client)
            .collect()
    }

    /// Marks a context focused (or not). Focusing records when, so the most
    /// recently focused context wins if a client ever reports two.
    pub fn set_focus(&self, ic: &Ic, focused: bool) {
        if focused {
            let stamp = self.focus_clock.fetch_add(1, Ordering::Relaxed);
            ic.focus_stamp.store(stamp, Ordering::Relaxed);
        }
        ic.focused.store(focused, Ordering::Relaxed);
    }

    /// The context text should go to, if any field has focus.
    pub fn focused(&self) -> Option<Arc<Ic>> {
        self.all()
            .into_iter()
            .filter(|ic| ic.focused.load(Ordering::Relaxed))
            .max_by_key(|ic| ic.focus_stamp.load(Ordering::Relaxed))
    }
}
