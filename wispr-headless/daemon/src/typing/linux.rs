//! Linux: wispr-ime, then keys, then the clipboard (see mod.rs for when).

use std::io::Write;
use std::process::{Command, Stdio};

use super::keymap::{Keymap, Step};
use super::uinput::Uinput;
use super::x11::X;
use super::{ime, Log, Method, Options};

pub struct Typist {
    method: Method,
    /// Native Wayland windows only take keys from uinput; XTEST would only
    /// reach XWayland's.
    wayland: bool,
    x: Option<X>,
    uinput: Option<Uinput>,
    wl_copy: String,
    log: Log,
}

impl Typist {
    pub fn new(options: Options, log: Log) -> Result<Typist, String> {
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        let method = options.method;
        let x = match X::connect() {
            Ok(x) => Some(x),
            Err(e) if wayland || method == Method::Ime => {
                log(format!("typing: no X display ({e}), so no keyboard layout"));
                None
            }
            Err(e) if method == Method::Auto || method == Method::Keys => return Err(e),
            Err(_) => None,
        };
        let uinput = if wayland && method != Method::Ime {
            match Uinput::new() {
                Ok(u) => Some(u),
                Err(e) if method == Method::Auto => {
                    log(format!("typing: {e}; only wispr-ime can type"));
                    None
                }
                Err(e) => return Err(e),
            }
        } else {
            None
        };
        Ok(Typist {
            method,
            wayland,
            x,
            uinput,
            wl_copy: options.wl_copy.unwrap_or_else(|| "wl-copy".into()),
            log,
        })
    }

    pub fn type_text(&mut self, text: &str) -> Result<(), String> {
        if matches!(self.method, Method::Auto | Method::Ime) {
            match ime::commit(text) {
                Ok(true) => return Ok(()),
                Ok(false) if self.method == Method::Ime => {
                    return Err("wispr-ime: no field it serves has focus".into())
                }
                Err(e) if self.method == Method::Ime => return Err(e),
                _ => {}
            }
        }
        let keymap = match &self.x {
            Some(x) => Some(x.keymap()?),
            None => None,
        };
        if self.method == Method::Paste && self.wayland {
            return self.paste(text, keymap.as_ref());
        }
        let Some(keymap) = keymap else {
            // Wayland without XWayland: no layout to find the keys in.
            return self.paste(text, None);
        };
        let plan = keymap.plan(text);
        if !self.wayland {
            let x = self.x.as_ref().expect("a keymap comes from X");
            return x.type_plan(&keymap, &plan);
        }
        let missing = plan.iter().filter(|s| matches!(s, Step::Missing(_))).count();
        if missing > 0 {
            // Keys and a paste in one transcript could arrive out of order
            // (apps read the clipboard asynchronously), so all or nothing.
            match self.paste(text, Some(&keymap)) {
                Ok(()) => return Ok(()),
                Err(e) => (self.log)(format!("typing: {e}; typing what the layout has")),
            }
        }
        self.uinput
            .as_mut()
            .ok_or("typing on Wayland needs /dev/uinput")?
            .type_plan(&keymap, &plan)?;
        if missing > 0 {
            return Err(format!(
                "{missing} characters are not in the keyboard layout; left out"
            ));
        }
        Ok(())
    }

    /// wl-copy, then Ctrl+V. The clipboard keeps the transcript.
    fn paste(&mut self, text: &str, keymap: Option<&Keymap>) -> Result<(), String> {
        let uinput = self
            .uinput
            .as_mut()
            .ok_or("pasting on Wayland needs /dev/uinput")?;
        copy(&self.wl_copy, text)?;
        uinput.ctrl_v(keymap)
    }
}

fn copy(wl_copy: &str, text: &str) -> Result<(), String> {
    let mut child = Command::new(wl_copy)
        .args(["--type", "text/plain;charset=utf-8"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot run {wl_copy}: {e}"))?;
    // Dropped (closed) before the wait, so wl-copy sees the end.
    let written = child
        .stdin
        .take()
        .map_or(Ok(()), |mut stdin| stdin.write_all(text.as_bytes()));
    if written.is_err() {
        let _ = child.kill();
    }
    // Always reaped. wl-copy leaves a process of its own behind to serve the
    // clipboard (not our child) and returns once that owns it.
    let status = child.wait();
    written.map_err(|e| format!("{wl_copy}: {e}"))?;
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("{wl_copy} failed: {status}")),
        Err(e) => Err(format!("{wl_copy}: {e}")),
    }
}
