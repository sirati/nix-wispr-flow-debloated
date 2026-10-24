//! The X server: its keyboard layout for keymap.rs (on Wayland, XWayland's
//! copy of the compositor's), and on X11 typing through XTEST, which needs
//! no device access. A character the layout lacks is typed the way xdotool
//! does it: a keycode without keysyms is lent that character for a moment
//! and given back afterwards.

use std::time::Duration;

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xkb::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{ConnectionExt as _, ModMask, KEY_PRESS_EVENT, KEY_RELEASE_EVENT};
use x11rb::protocol::xtest::{self, ConnectionExt as _};
use x11rb::rust_connection::RustConnection;

use super::keymap::{Keymap, Step, Stroke};
use super::{keysyms, KEY_GAP};

/// Clients reread the mapping when it changes; this gives them time to
/// before keys use a lent keycode, and after, before it is given back.
const REMAP_SETTLE: Duration = Duration::from_millis(30);

pub struct X {
    conn: RustConnection,
    xkb: bool,
    xtest: bool,
}

fn err(e: impl std::fmt::Display) -> String {
    format!("X: {e}")
}

impl X {
    pub fn connect() -> Result<X, String> {
        let (conn, _) = x11rb::connect(None).map_err(err)?;
        let xkb = conn
            .xkb_use_extension(1, 0)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|r| r.supported);
        let xtest = conn
            .extension_information(xtest::X11_EXTENSION_NAME)
            .ok()
            .flatten()
            .is_some();
        Ok(X { conn, xkb, xtest })
    }

    /// The layout as it is now: the active group and Caps Lock count.
    pub fn keymap(&self) -> Result<Keymap, String> {
        let setup = self.conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let mapping = self
            .conn
            .get_keyboard_mapping(min, max - min + 1)
            .map_err(err)?
            .reply()
            .map_err(err)?;
        let (group, caps) = if self.xkb {
            let state = self
                .conn
                .xkb_get_state(xkb::ID::USE_CORE_KBD.into())
                .map_err(err)?
                .reply()
                .map_err(err)?;
            (
                u8::from(state.group),
                state.locked_mods.contains(ModMask::LOCK),
            )
        } else {
            (0, false)
        };
        Ok(Keymap::from_core(
            min,
            mapping.keysyms_per_keycode,
            &mapping.keysyms,
            group,
            caps,
        ))
    }

    fn fake(&self, keycode: u8, down: bool) -> Result<(), String> {
        let kind = if down { KEY_PRESS_EVENT } else { KEY_RELEASE_EVENT };
        self.conn
            .xtest_fake_input(kind, keycode, x11rb::CURRENT_TIME, x11rb::NONE, 0, 0, 0)
            .map_err(err)?;
        Ok(())
    }

    fn stroke(&self, s: Stroke, keymap: &Keymap) -> Result<(), String> {
        let mut mods = Vec::with_capacity(2);
        if s.shift {
            mods.push(keymap.shift);
        }
        if s.level3 {
            mods.extend(keymap.level3);
        }
        for &m in &mods {
            self.fake(m, true)?;
        }
        let pressed = self.fake(s.keycode, true).and_then(|()| self.fake(s.keycode, false));
        for &m in mods.iter().rev() {
            let _ = self.fake(m, false);
        }
        self.conn.flush().map_err(err)?;
        pressed
    }

    /// Waits until the server has handled everything sent so far.
    fn sync(&self) -> Result<(), String> {
        self.conn
            .get_input_focus()
            .map_err(err)?
            .reply()
            .map(|_| ())
            .map_err(err)
    }

    /// Points `keycode` at `keysym` on every level (0: no keysym).
    fn lend(&self, keymap: &Keymap, keycode: u8, keysym: u32) -> Result<(), String> {
        let row = vec![keysym; usize::from(keymap.per_keycode)];
        self.conn
            .change_keyboard_mapping(1, keycode, keymap.per_keycode, &row)
            .map_err(err)?;
        Ok(())
    }

    /// Types `plan` through XTEST. Characters the layout lacks borrow the
    /// spare keycodes, as many at a time as there are; with none spare they
    /// are left out.
    pub fn type_plan(&self, keymap: &Keymap, plan: &[Step]) -> Result<(), String> {
        if !self.xtest {
            return Err("the X server has no XTEST".into());
        }
        let mut lent: Vec<(char, u8)> = Vec::new();
        // How many spare keycodes were lent at most.
        let mut used = 0;
        let mut skipped = 0;
        let mut result = Ok(());
        for step in plan {
            let stroke = match *step {
                Step::Key(s) => s,
                Step::Missing(c) => match lent.iter().find(|l| l.0 == c) {
                    Some(&(_, keycode)) => Stroke {
                        keycode,
                        shift: false,
                        level3: false,
                    },
                    None if keymap.spare.is_empty() => {
                        skipped += 1;
                        continue;
                    }
                    None => {
                        // All spare keycodes taken: the keys sent so far
                        // must land before they are lent again.
                        if lent.len() == keymap.spare.len() {
                            self.sync()?;
                            std::thread::sleep(REMAP_SETTLE);
                            lent.clear();
                        }
                        let keycode = keymap.spare[lent.len()];
                        self.lend(keymap, keycode, keysyms::of_char(c))?;
                        self.sync()?;
                        std::thread::sleep(REMAP_SETTLE);
                        lent.push((c, keycode));
                        used = used.max(lent.len());
                        Stroke {
                            keycode,
                            shift: false,
                            level3: false,
                        }
                    }
                },
            };
            if let Err(e) = self.stroke(stroke, keymap) {
                result = Err(e);
                break;
            }
            std::thread::sleep(KEY_GAP);
        }
        if used > 0 {
            // Given back once the keys that used them have been handled.
            let _ = self.sync();
            std::thread::sleep(REMAP_SETTLE);
            for &keycode in &keymap.spare[..used] {
                let _ = self.lend(keymap, keycode, 0);
            }
            let _ = self.sync();
        }
        result?;
        if skipped > 0 {
            return Err(format!(
                "{skipped} characters are not in the keyboard layout and no keycode is free; left out"
            ));
        }
        Ok(())
    }
}
