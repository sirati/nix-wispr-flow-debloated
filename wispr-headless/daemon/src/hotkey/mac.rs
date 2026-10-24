//! macOS: the keys from a listen-only event tap on the login session, on a
//! thread with its own run loop (AppKit keeps the main one for the bar). The
//! tap only listens: every key still reaches the app it was meant for, and a
//! slow tap cannot hold up typing.
//!
//! macOS asks for Accessibility or Input Monitoring before a process may
//! tap keys. wispr-headless runs as a child of Wispr Flow, so the system
//! counts it as Wispr, which already has Accessibility for its own shortcut
//! (darwin/wispr-flow.nix signs the app so the permission lasts).
//!
//! Modifiers only send FlagsChanged events, which carry the state of all
//! modifiers; keys.rs reads which one changed. Fn and Globe are the same key
//! (63). Events this process types itself (typing/mac.rs) are marked and
//! skipped.
//!
//! The tap's callback is a C function, so this module allows unsafe code.
//! Each block says why it is sound.
#![allow(unsafe_code)]

use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::mpsc;

use objc2_core_foundation::{kCFRunLoopCommonModes, CFMachPort, CFRetained, CFRunLoop};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventMask, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
    CGEventTapOptions, CGEventTapPlacement, CGEventTapProxy, CGEventType,
};

use super::keys::{mac_modifier_down, MAC_MODIFIERS};
use super::{Key, KeySink, Log};

struct Tap {
    port: CFRetained<CFMachPort>,
    keys: KeySink,
    log: Log,
    /// Modifiers reported down.
    modifiers: Vec<Key>,
    /// Other keys reported down.
    others: Vec<Key>,
}

thread_local! {
    /// The tap, owned by its thread; the callback runs there.
    static TAP: RefCell<Option<Tap>> = const { RefCell::new(None) };
}

pub fn watch(_wanted: &[Key], keys: KeySink, log: Log) -> Result<(), String> {
    let (ready, started) = mpsc::channel();
    std::thread::Builder::new()
        .name("hotkey-tap".into())
        .spawn(move || {
            let mask: CGEventMask = [
                CGEventType::KeyDown,
                CGEventType::KeyUp,
                CGEventType::FlagsChanged,
            ]
            .iter()
            .fold(0, |m, t| m | 1 << t.0);
            // SAFETY: `callback` has the signature CGEventTapCallBack asks
            // for and touches only this thread's TAP; the user info pointer
            // is null and never read.
            let port = unsafe {
                CGEvent::tap_create(
                    CGEventTapLocation::SessionEventTap,
                    CGEventTapPlacement::HeadInsertEventTap,
                    CGEventTapOptions::ListenOnly,
                    mask,
                    Some(callback),
                    std::ptr::null_mut(),
                )
            };
            let Some(port) = port else {
                let _ = ready.send(Err(
                    "cannot tap the keyboard: Wispr Flow needs Accessibility \
                     (or Input Monitoring) in System Settings > Privacy & Security"
                        .to_string(),
                ));
                return;
            };
            let (Some(source), Some(run_loop)) = (
                CFMachPort::new_run_loop_source(None, Some(&port), 0),
                CFRunLoop::current(),
            ) else {
                let _ = ready.send(Err("cannot run the keyboard tap".to_string()));
                return;
            };
            // SAFETY: kCFRunLoopCommonModes is an immutable constant of
            // CoreFoundation, valid for the whole process.
            run_loop.add_source(Some(&source), unsafe { kCFRunLoopCommonModes });
            TAP.with(|t| {
                *t.borrow_mut() = Some(Tap {
                    port,
                    keys,
                    log,
                    modifiers: Vec::new(),
                    others: Vec::new(),
                })
            });
            let _ = ready.send(Ok(()));
            CFRunLoop::run();
        })
        .map_err(|e| format!("cannot start the keyboard tap: {e}"))?;
    started
        .recv()
        .map_err(|_| "the keyboard tap ended".to_string())?
}

/// Called by the run loop for each event.
unsafe extern "C-unwind" fn callback(
    _proxy: CGEventTapProxy,
    kind: CGEventType,
    event: NonNull<CGEvent>,
    _user_info: *mut c_void,
) -> *mut CGEvent {
    // SAFETY: the tap hands us a valid event that lives at least until we
    // return.
    let e = unsafe { event.as_ref() };
    TAP.with(|t| {
        if let Ok(mut t) = t.try_borrow_mut() {
            if let Some(tap) = t.as_mut() {
                tap.handle(kind, e);
            }
        }
    });
    // A listen-only tap passes every event on whatever we return.
    event.as_ptr()
}

impl Tap {
    fn send(&self, key: Key, down: bool) {
        let _ = self.keys.send((key, down));
    }

    fn handle(&mut self, kind: CGEventType, e: &CGEvent) {
        if kind == CGEventType::TapDisabledByTimeout || kind == CGEventType::TapDisabledByUserInput
        {
            // Events may have been missed while it was off.
            CGEvent::tap_enable(&self.port, true);
            (self.log)("shortcut: the keyboard tap was off; on again".into());
            self.resync();
            return;
        }
        if CGEvent::integer_value_field(Some(e), CGEventField::EventSourceUserData)
            == crate::typing::MAC_EVENT_MARK
        {
            return;
        }
        let key = CGEvent::integer_value_field(Some(e), CGEventField::KeyboardEventKeycode) as Key;
        let flags = CGEvent::flags(Some(e)).0;
        if kind == CGEventType::FlagsChanged {
            // The changed key first, in case its flags cannot tell.
            let held = self.modifiers.contains(&key);
            match mac_modifier_down(key, flags) {
                // It changed and is down, so its release was missed (the
                // tap was off, or events were coalesced): report it, so a
                // dictation the release should have ended ends now.
                Some(true) if held => {
                    (self.log)("shortcut: a modifier's release was missed".into());
                    self.modifier(key, false);
                    self.modifier(key, true);
                }
                Some(down) => self.modifier(key, down),
                // A kind bit without a side bit: ask the system. (Guessing
                // that it flipped turns press and release round for good
                // once one event is missed.)
                None if MAC_MODIFIERS.iter().any(|m| m.0 == key) => {
                    let down =
                        CGEventSource::key_state(CGEventSourceStateID::HIDSystemState, key);
                    if down && held {
                        self.modifier(key, false);
                    }
                    self.modifier(key, down);
                }
                None => {}
            }
            for &(m, ..) in MAC_MODIFIERS {
                if m != key {
                    if let Some(down) = mac_modifier_down(m, flags) {
                        self.modifier(m, down);
                    }
                }
            }
        } else if kind == CGEventType::KeyDown || kind == CGEventType::KeyUp {
            let down = kind == CGEventType::KeyDown;
            let repeat =
                CGEvent::integer_value_field(Some(e), CGEventField::KeyboardEventAutorepeat) != 0;
            if down && repeat {
                return;
            }
            // Only releases from these flags: arrow and function keys carry
            // the Fn bit whether Fn is down or not.
            for &(m, ..) in MAC_MODIFIERS {
                if mac_modifier_down(m, flags) == Some(false) {
                    self.modifier(m, false);
                }
            }
            if down && !self.others.contains(&key) {
                self.others.push(key);
            } else if !down {
                self.others.retain(|&k| k != key);
            }
            self.send(key, down);
        }
    }

    fn modifier(&mut self, key: Key, down: bool) {
        if self.modifiers.contains(&key) == down {
            return;
        }
        if down {
            self.modifiers.push(key);
        } else {
            self.modifiers.retain(|&k| k != key);
        }
        self.send(key, down);
    }

    /// Releases the keys that are no longer down after the tap was off.
    fn resync(&mut self) {
        let up = |k: &Key| !CGEventSource::key_state(CGEventSourceStateID::HIDSystemState, *k);
        let modifiers: Vec<Key> = self.modifiers.iter().copied().filter(up).collect();
        for k in modifiers {
            self.modifier(k, false);
        }
        let others: Vec<Key> = self.others.iter().copied().filter(up).collect();
        for k in others {
            self.others.retain(|&o| o != k);
            self.send(k, false);
        }
    }
}
