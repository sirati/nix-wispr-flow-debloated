//! macOS: a transcript as key events that carry their text, or pasted with
//! Cmd+V. Posting events needs Accessibility, which Wispr Flow has and this
//! child of it shares (see hotkey/mac.rs). Every event is marked so the
//! shortcut tap skips it.
//!
//! A paste puts back what the clipboard held, half a second later, unless
//! something else changed it in the meantime. The transcript is marked
//! transient for clipboard managers (nspasteboard.org). Cmd+V is the key
//! where QWERTY has V; layouts that move V (Dvorak, not Dvorak-QWERTY ⌘)
//! would need the layout read.
//!
//! CGEventKeyboardSetUnicodeString takes a pointer and AppKit's pasteboard
//! types are extern statics, so this module allows unsafe code. Each block
//! says why it is sound.
#![allow(unsafe_code)]

use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{NSPasteboard, NSPasteboardItem, NSPasteboardTypeString, NSPasteboardWriting};
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
    CGPreflightPostEventAccess,
};
use objc2_foundation::{NSArray, NSData, NSString};

use super::{mac_steps, Log, MacStep, Method, Options, KEY_GAP, MAC_CHUNK, MAC_EVENT_MARK};

/// kVK_Return, kVK_ANSI_V.
const RETURN: u16 = 36;
const V: u16 = 9;
/// How long the app gets to read the clipboard before it is put back.
const PASTE_SETTLE: Duration = Duration::from_millis(500);

pub struct Typist {
    method: Method,
}

impl Typist {
    pub fn new(options: Options, log: Log) -> Result<Typist, String> {
        if options.method == Method::Ime {
            return Err("macOS has no input method path; use keys or paste".into());
        }
        if !CGPreflightPostEventAccess() {
            log("typing: Wispr Flow needs Accessibility to type".into());
        }
        Ok(Typist {
            method: options.method,
        })
    }

    pub fn type_text(&mut self, text: &str) -> Result<(), String> {
        let source = CGEventSource::new(CGEventSourceStateID::Private)
            .ok_or("cannot create an event source")?;
        match self.method {
            Method::Keys => keys(&source, text),
            _ => paste(&source, text),
        }
    }
}

fn post(event: &CGEvent) {
    CGEvent::set_integer_value_field(Some(event), CGEventField::EventSourceUserData, MAC_EVENT_MARK);
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(event));
}

fn event(source: &CGEventSource, key: u16, down: bool) -> Result<CFRetained<CGEvent>, String> {
    CGEvent::new_keyboard_event(Some(source), key, down).ok_or_else(|| "cannot create a key event".into())
}

/// Presses and releases `key` with exactly `flags` (none of the modifiers
/// the user may still hold).
fn press(source: &CGEventSource, key: u16, flags: CGEventFlags) -> Result<(), String> {
    for down in [true, false] {
        let e = event(source, key, down)?;
        CGEvent::set_flags(Some(&e), flags);
        post(&e);
    }
    Ok(())
}

fn keys(source: &CGEventSource, text: &str) -> Result<(), String> {
    for step in mac_steps(text, MAC_CHUNK) {
        match step {
            MacStep::Newline => press(source, RETURN, CGEventFlags::MaskShift)?,
            MacStep::Text(units) => {
                for down in [true, false] {
                    // The key code does not matter: the text replaces what
                    // it would type.
                    let e = event(source, 0, down)?;
                    CGEvent::set_flags(Some(&e), CGEventFlags::empty());
                    // SAFETY: `units` outlives the call, which copies
                    // `units.len()` UTF-16 units from it.
                    unsafe {
                        CGEvent::keyboard_set_unicode_string(Some(&e), units.len() as _, units.as_ptr())
                    };
                    post(&e);
                }
            }
        }
        std::thread::sleep(KEY_GAP);
    }
    Ok(())
}

/// Each item on the clipboard, as its types and their data.
type Saved = Vec<Vec<(Retained<NSString>, Retained<NSData>)>>;

fn save(pasteboard: &NSPasteboard) -> Saved {
    let Some(items) = pasteboard.pasteboardItems() else {
        return Vec::new();
    };
    items
        .iter()
        .map(|item| {
            item.types()
                .iter()
                .filter_map(|t| {
                    let data = item.dataForType(&t)?;
                    Some((t, data))
                })
                .collect()
        })
        .collect()
}

fn restore(pasteboard: &NSPasteboard, saved: Saved) {
    pasteboard.clearContents();
    if saved.is_empty() {
        return;
    }
    let items: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = saved
        .into_iter()
        .map(|types| {
            let item = NSPasteboardItem::new();
            for (t, data) in types {
                item.setData_forType(&data, &t);
            }
            ProtocolObject::from_retained(item)
        })
        .collect();
    pasteboard.writeObjects(&NSArray::from_retained_slice(&items));
}

fn paste(source: &CGEventSource, text: &str) -> Result<(), String> {
    let pasteboard = NSPasteboard::generalPasteboard();
    let saved = save(&pasteboard);
    // SAFETY: an AppKit constant, immutable and valid for the whole process.
    let string = unsafe { NSPasteboardTypeString };
    let item = NSPasteboardItem::new();
    item.setString_forType(&NSString::from_str(text), string);
    for marker in [
        "org.nspasteboard.TransientType",
        "org.nspasteboard.AutoGeneratedType",
    ] {
        item.setString_forType(&NSString::from_str(""), &NSString::from_str(marker));
    }
    pasteboard.clearContents();
    let items = [ProtocolObject::<dyn NSPasteboardWriting>::from_retained(item)];
    if !pasteboard.writeObjects(&NSArray::from_retained_slice(&items)) {
        restore(&pasteboard, saved);
        return Err("cannot put the transcript on the clipboard".into());
    }
    let ours = pasteboard.changeCount();
    press(source, V, CGEventFlags::MaskCommand)?;
    std::thread::sleep(PASTE_SETTLE);
    if pasteboard.changeCount() == ours {
        restore(&pasteboard, saved);
    }
    Ok(())
}
