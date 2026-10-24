//! A virtual keyboard through /dev/uinput: the compositor reads it like any
//! keyboard and applies its own layout, so it types into native Wayland
//! windows on any compositor, where XTEST only reaches X clients. Wispr's
//! helper types its Ctrl+V the same way. Needs write access to /dev/uinput
//! (Wispr's udev rule gives it to the `input` group and the active seat).

use std::time::{Duration, Instant};

use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, EventType, InputEvent, KeyCode};

use super::keymap::{Keymap, Step, Stroke, EVDEV_OFFSET};
use super::{DEVICE_NAME, KEY_GAP};

/// How long a compositor takes to start reading a new device; keys sent
/// before then are lost.
const SETTLE: Duration = Duration::from_millis(300);

pub struct Uinput {
    device: VirtualDevice,
    ready: Instant,
}

impl Uinput {
    pub fn new() -> Result<Uinput, String> {
        // Every key an X keycode can name (8..=255, evdev 0..=247), so
        // whatever the layout puts its characters on can be pressed.
        let mut keys = AttributeSet::<KeyCode>::new();
        for code in 1..=247 {
            keys.insert(KeyCode::new(code));
        }
        let device = VirtualDevice::builder()
            .and_then(|b| b.name(DEVICE_NAME).with_keys(&keys))
            .and_then(|b| b.build())
            .map_err(|e| format!("cannot create a uinput keyboard: {e}"))?;
        Ok(Uinput {
            device,
            ready: Instant::now() + SETTLE,
        })
    }

    fn key(&mut self, keycode: u8, down: bool) -> Result<(), String> {
        let code = u16::from(keycode.saturating_sub(EVDEV_OFFSET));
        self.device
            .emit(&[InputEvent::new(EventType::KEY.0, code, i32::from(down))])
            .map_err(|e| format!("uinput: {e}"))
    }

    /// Modifiers, key, and their releases in one go: KWin drops a virtual
    /// modifier when nothing follows it for a few milliseconds, which turned
    /// the helper's Ctrl+V into a bare v (0 ms worked, 8 ms did not).
    fn stroke(&mut self, s: Stroke, keymap: &Keymap) -> Result<(), String> {
        let mut mods = Vec::with_capacity(2);
        if s.shift {
            mods.push(keymap.shift);
        }
        if s.level3 {
            mods.extend(keymap.level3);
        }
        for &m in &mods {
            self.key(m, true)?;
        }
        let pressed = self.key(s.keycode, true).and_then(|()| self.key(s.keycode, false));
        // Released even if the key failed, so no modifier stays stuck.
        for &m in mods.iter().rev() {
            let _ = self.key(m, false);
        }
        pressed
    }

    fn wait_until_ready(&self) {
        let left = self.ready.saturating_duration_since(Instant::now());
        if !left.is_zero() {
            std::thread::sleep(left);
        }
    }

    /// Types the keys of `plan`; missing characters are skipped.
    pub fn type_plan(&mut self, keymap: &Keymap, plan: &[Step]) -> Result<(), String> {
        self.wait_until_ready();
        for step in plan {
            if let Step::Key(s) = *step {
                self.stroke(s, keymap)?;
                std::thread::sleep(KEY_GAP);
            }
        }
        Ok(())
    }

    /// Ctrl+V, with the layout's V if it is known.
    pub fn ctrl_v(&mut self, keymap: Option<&Keymap>) -> Result<(), String> {
        self.wait_until_ready();
        let (control, v) = keymap.map_or((29 + EVDEV_OFFSET, 47 + EVDEV_OFFSET), |k| {
            (k.control, k.v())
        });
        self.key(control, true)?;
        let pressed = self.key(v, true).and_then(|()| self.key(v, false));
        let _ = self.key(control, false);
        pressed
    }
}
