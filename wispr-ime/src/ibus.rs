//! What mutter (or any other libibus client) talks to: the `org.freedesktop.IBus`
//! object that creates input contexts, and each context's interface. Every
//! call is passed on to fcitx5; whatever fcitx5 wants done comes back as
//! signals sent to the client that owns the context.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use tracing::{debug, warn};
use zbus::message::Header;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{fdo, interface, Connection};

use crate::fcitx::{self, Event};
use crate::state::{Ic, Shared};
use crate::wire::{self, RELEASE_MASK};

const IBUS_IC_IFACE: &str = "org.freedesktop.IBus.InputContext";

/// Sends `member` to the client that owns `ic`, and nobody else.
async fn emit<B>(conn: &Connection, ic: &Ic, member: &str, body: &B)
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    if let Err(e) = conn
        .emit_signal(
            Some(ic.client.as_str()),
            &ic.ibus_path,
            IBUS_IC_IFACE,
            member,
            body,
        )
        .await
    {
        warn!("sending {member} to {} failed: {e}", ic.client);
    }
}

/// Commits text into the client's focused field.
pub async fn commit_text(conn: &Connection, ic: &Ic, text: &str) {
    emit(conn, ic, "CommitText", &(wire::ibus_text(text),)).await;
}

/// Carries out one thing fcitx5 asked for.
pub async fn deliver(shared: &Shared, ic: &Ic, event: Event) {
    let conn = &shared.conn;
    match event {
        Event::Commit(text) => commit_text(conn, ic, &text).await,
        Event::Preedit { segments, cursor } => {
            let p = wire::preedit(&segments, cursor);
            if ic.client_commit_preedit.load(Ordering::Relaxed) {
                emit(
                    conn,
                    ic,
                    "UpdatePreeditTextWithMode",
                    &(p.text, p.cursor, p.visible, p.mode),
                )
                .await;
            } else {
                emit(
                    conn,
                    ic,
                    "UpdatePreeditText",
                    &(p.text, p.cursor, p.visible),
                )
                .await;
            }
        }
        Event::Forward {
            sym,
            states,
            release,
        } => {
            let state = if release {
                states | RELEASE_MASK
            } else {
                states
            };
            emit(
                conn,
                ic,
                "ForwardKeyEvent",
                &(sym, ic.keycode_for(sym), state),
            )
            .await;
        }
        Event::Delete { offset, size } => {
            emit(conn, ic, "DeleteSurroundingText", &(offset, size)).await;
        }
    }
}

/// Sends the capability word to fcitx5.
async fn push_caps(shared: &Shared, ic: &Ic) {
    let _ = fcitx::call(
        &shared.conn,
        &ic.fcitx_path,
        "SetCapability",
        &(ic.fcitx_caps(),),
    )
    .await;
}

/// True if the call came from the client that created `ic`. Anyone else on
/// the session bus must not drive another application's input.
fn from_owner(header: &Header<'_>, ic: &Ic) -> bool {
    header.sender().is_some_and(|s| s.as_str() == ic.client)
}

/// `org.freedesktop.IBus` at `/org/freedesktop/IBus`.
pub struct IBusBus {
    pub shared: Arc<Shared>,
}

#[interface(name = "org.freedesktop.IBus")]
impl IBusBus {
    async fn create_input_context(
        &self,
        name: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<OwnedObjectPath> {
        let client = header
            .sender()
            .ok_or_else(|| fdo::Error::Failed("no sender".into()))?
            .to_string();
        let program = if name.is_empty() || name == "QIBusInputContext" || name == "gtk-im" {
            "ibus-client"
        } else {
            name
        };
        let conn = &self.shared.conn;
        let fcitx_path = fcitx::create_ic(conn, program)
            .await
            .map_err(|e| fdo::Error::Failed(format!("fcitx5: {e}")))?
            .to_string();
        let id = self.shared.allocate_id();
        let ibus_path =
            OwnedObjectPath::try_from(format!("/org/freedesktop/IBus/InputContext_{id}"))
                .map_err(|e| fdo::Error::Failed(e.to_string()))?;
        let ic = Arc::new(Ic::new(id, client, ibus_path.clone(), fcitx_path));
        self.shared.insert(ic.clone());
        let server = conn.object_server();
        let registered = async {
            server
                .at(
                    &ibus_path,
                    InputContextIface {
                        shared: self.shared.clone(),
                        ic: ic.clone(),
                    },
                )
                .await?;
            server
                .at(
                    &ibus_path,
                    ServiceIface {
                        shared: self.shared.clone(),
                        ic: ic.clone(),
                    },
                )
                .await?;
            zbus::Result::Ok(())
        }
        .await;
        if let Err(e) = registered {
            destroy(&self.shared, &ic).await;
            return Err(fdo::Error::Failed(e.to_string()));
        }
        debug!("input context {id} for {program} ({})", ic.client);
        Ok(ibus_path)
    }
}

/// Removes a context everywhere: our object, our bookkeeping, and fcitx5's.
pub async fn destroy(shared: &Shared, ic: &Ic) {
    shared.remove(ic.id);
    let server = shared.conn.object_server();
    let _ = server.remove::<InputContextIface, _>(&ic.ibus_path).await;
    let _ = server.remove::<ServiceIface, _>(&ic.ibus_path).await;
    let _ = fcitx::call(&shared.conn, &ic.fcitx_path, "DestroyIC", &()).await;
    debug!("input context {} destroyed", ic.id);
}

/// `org.freedesktop.IBus.Service`: lets the client end a context.
pub struct ServiceIface {
    shared: Arc<Shared>,
    ic: Arc<Ic>,
}

#[interface(name = "org.freedesktop.IBus.Service")]
impl ServiceIface {
    async fn destroy(&self, #[zbus(header)] header: Header<'_>) {
        if from_owner(&header, &self.ic) {
            destroy(&self.shared, &self.ic).await;
        }
    }
}

/// `org.freedesktop.IBus.InputContext`.
pub struct InputContextIface {
    shared: Arc<Shared>,
    ic: Arc<Ic>,
}

#[interface(name = "org.freedesktop.IBus.InputContext")]
impl InputContextIface {
    async fn process_key_event(
        &self,
        keyval: u32,
        keycode: u32,
        state: u32,
        #[zbus(header)] header: Header<'_>,
    ) -> bool {
        if !from_owner(&header, &self.ic) {
            return false;
        }
        let _order = self.ic.gate.lock().await;
        self.ic.remember_keycode(keyval, keycode);
        // fcitx5 forces focus on a key event, so it counts as focus here too.
        if !self.ic.focused.load(Ordering::Relaxed) {
            self.shared.set_focus(&self.ic, true);
        }
        let release = state & RELEASE_MASK != 0;
        let result = fcitx::process_key(
            &self.shared.conn,
            &self.ic.fcitx_path,
            keyval,
            // IBus sends evdev keycodes, fcitx5 expects X keycodes.
            keycode + 8,
            state & !RELEASE_MASK,
            release,
        )
        .await;
        match result {
            Ok((events, handled)) => {
                for event in events {
                    deliver(&self.shared, &self.ic, event).await;
                }
                handled
            }
            Err(e) => {
                warn!("fcitx5 key event failed: {e}");
                false
            }
        }
    }

    async fn set_cursor_location(
        &self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        #[zbus(header)] header: Header<'_>,
    ) {
        self.cursor(x, y, w, h, false, &header).await;
    }

    async fn set_cursor_location_relative(
        &self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        #[zbus(header)] header: Header<'_>,
    ) {
        self.cursor(x, y, w, h, true, &header).await;
    }

    async fn focus_in(&self, #[zbus(header)] header: Header<'_>) {
        if !from_owner(&header, &self.ic) {
            return;
        }
        let _order = self.ic.gate.lock().await;
        self.shared.set_focus(&self.ic, true);
        let _ = fcitx::call(&self.shared.conn, &self.ic.fcitx_path, "FocusIn", &()).await;
    }

    async fn focus_out(&self, #[zbus(header)] header: Header<'_>) {
        if !from_owner(&header, &self.ic) {
            return;
        }
        let _order = self.ic.gate.lock().await;
        self.shared.set_focus(&self.ic, false);
        let _ = fcitx::call(&self.shared.conn, &self.ic.fcitx_path, "FocusOut", &()).await;
    }

    async fn reset(&self, #[zbus(header)] header: Header<'_>) {
        if !from_owner(&header, &self.ic) {
            return;
        }
        let _order = self.ic.gate.lock().await;
        let _ = fcitx::call(&self.shared.conn, &self.ic.fcitx_path, "Reset", &()).await;
    }

    fn enable(&self) {}

    fn disable(&self) {}

    fn is_enabled(&self) -> bool {
        true
    }

    async fn set_capabilities(&self, caps: u32, #[zbus(header)] header: Header<'_>) {
        if !from_owner(&header, &self.ic) {
            return;
        }
        let _order = self.ic.gate.lock().await;
        let before = self.ic.ibus_caps.swap(caps, Ordering::Relaxed);
        push_caps(&self.shared, &self.ic).await;
        // A client that just started supporting surrounding text is asked to
        // send it, as fcitx5's own IBus frontend does.
        let wants = wire::ibus_cap::SURROUNDING_TEXT;
        if caps & wants != 0 && before & wants == 0 {
            emit(&self.shared.conn, &self.ic, "RequireSurroundingText", &()).await;
        }
    }

    fn property_activate(&self, _name: &str, _state: i32) {}

    fn set_engine(&self, _engine: &str) {}

    fn get_engine(&self) -> Value<'static> {
        Value::from(0i32)
    }

    async fn set_surrounding_text(
        &self,
        text: Value<'_>,
        cursor: u32,
        anchor: u32,
        #[zbus(header)] header: Header<'_>,
    ) {
        if !from_owner(&header, &self.ic) {
            return;
        }
        let Some(text) = wire::text_of(&text) else {
            return;
        };
        let _order = self.ic.gate.lock().await;
        let _ = fcitx::call(
            &self.shared.conn,
            &self.ic.fcitx_path,
            "SetSurroundingText",
            &(text, cursor, anchor),
        )
        .await;
    }

    #[zbus(property)]
    fn content_type(&self) -> (u32, u32) {
        (0, 0)
    }

    #[zbus(property)]
    async fn set_content_type(&self, value: (u32, u32)) {
        let flags = wire::content_type_flags(value.0, value.1);
        self.ic.content_flags.store(flags, Ordering::Relaxed);
        push_caps(&self.shared, &self.ic).await;
    }

    #[zbus(property)]
    fn client_commit_preedit(&self) -> (bool,) {
        (self.ic.client_commit_preedit.load(Ordering::Relaxed),)
    }

    #[zbus(property)]
    fn set_client_commit_preedit(&self, value: (bool,)) {
        self.ic
            .client_commit_preedit
            .store(value.0, Ordering::Relaxed);
    }

    #[zbus(property)]
    fn effective_post_process_key_event(&self) -> (bool,) {
        (false,)
    }

    #[zbus(property)]
    fn set_effective_post_process_key_event(&self, _value: (bool,)) {}

    #[zbus(property)]
    fn post_process_key_event(&self) -> Vec<(u8, OwnedValue)> {
        Vec::new()
    }
}

impl InputContextIface {
    async fn cursor(&self, x: i32, y: i32, w: i32, h: i32, relative: bool, header: &Header<'_>) {
        if !from_owner(header, &self.ic) {
            return;
        }
        let _order = self.ic.gate.lock().await;
        if self.ic.relative_rect.swap(relative, Ordering::Relaxed) != relative {
            push_caps(&self.shared, &self.ic).await;
        }
        let _ = fcitx::call(
            &self.shared.conn,
            &self.ic.fcitx_path,
            "SetCursorRect",
            &(x, y, w, h),
        )
        .await;
    }
}
