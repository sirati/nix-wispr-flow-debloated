//! Encoding and decoding of IBus's wire types.
//!
//! IBus wraps every string in an `IBusText`, a D-Bus struct `(sa{sv}sv)` whose
//! last field is a variant holding an `IBusAttrList` `(sa{sv}av)`; each
//! attribute is an `IBusAttribute` `(sa{sv}uuuu)`. Layout and attribute codes
//! follow what fcitx5's own IBus frontend sends, which GNOME already accepts.

use std::collections::HashMap;

use zbus::zvariant::{Array, Signature, Structure, StructureBuilder, Value};

/// fcitx5 `TextFormatFlag` bits, as they arrive in `UpdateFormattedPreedit`.
pub const FORMAT_UNDERLINE: i32 = 1 << 3;
pub const FORMAT_HIGHLIGHT: i32 = 1 << 4;
pub const FORMAT_DONT_COMMIT: i32 = 1 << 5;

/// IBus `IBusAttrType` values.
const ATTR_UNDERLINE: u32 = 1;
const ATTR_FOREGROUND: u32 = 2;
const ATTR_BACKGROUND: u32 = 3;
/// `IBUS_ATTR_UNDERLINE_SINGLE`.
const UNDERLINE_SINGLE: u32 = 1;
const WHITE: u32 = 0x00ff_ffff;
const BLACK: u32 = 0;

/// Bit the IBus protocol uses in `state` to mark a key release.
pub const RELEASE_MASK: u32 = 1 << 30;

fn no_attachments() -> HashMap<String, Value<'static>> {
    HashMap::new()
}

fn attribute(kind: u32, value: u32, start: u32, end: u32) -> Value<'static> {
    let s = StructureBuilder::new()
        .add_field("IBusAttribute".to_string())
        .add_field(no_attachments())
        .add_field(kind)
        .add_field(value)
        .add_field(start)
        .add_field(end)
        .build()
        .expect("IBusAttribute has fields");
    Value::Structure(s)
}

fn attr_list(attrs: Vec<Value<'static>>) -> Value<'static> {
    // `av`: every element is a variant holding one IBusAttribute.
    let mut array = Array::new(&Signature::Variant);
    for a in attrs {
        array
            .append(Value::Value(Box::new(a)))
            .expect("every element is a variant");
    }
    let s = StructureBuilder::new()
        .add_field("IBusAttrList".to_string())
        .add_field(no_attachments())
        .add_field(array)
        .build()
        .expect("IBusAttrList has fields");
    Value::Structure(s)
}

fn text_struct(text: &str, attrs: Vec<Value<'static>>) -> Value<'static> {
    let s = StructureBuilder::new()
        .add_field("IBusText".to_string())
        .add_field(no_attachments())
        .add_field(text.to_string())
        .add_field(attr_list(attrs))
        .build()
        .expect("IBusText has fields");
    Value::Structure(s)
}

/// A plain `IBusText` without attributes, ready to be sent as a `v` argument.
pub fn ibus_text(text: &str) -> Value<'static> {
    text_struct(text, Vec::new())
}

/// One piece of fcitx5's formatted preedit: text plus `TextFormatFlag` bits.
pub type Segment = (String, i32);

/// What IBus needs to show a preedit string.
#[derive(Debug, PartialEq)]
pub struct Preedit {
    pub text: Value<'static>,
    /// Cursor position in characters.
    pub cursor: u32,
    pub visible: bool,
    /// `IBusPreeditFocusMode`: 1 commits the preedit when focus leaves, 0
    /// clears it. It is 0 only if every segment is marked DontCommit.
    pub mode: u32,
}

/// Converts fcitx5's formatted preedit (byte cursor) into IBus's form.
pub fn preedit(segments: &[Segment], byte_cursor: i32) -> Preedit {
    let mut full = String::new();
    let mut attrs = Vec::new();
    let mut offset: u32 = 0;
    let mut dont_commit = true;
    for (text, flags) in segments {
        let len = text.chars().count() as u32;
        if flags & FORMAT_UNDERLINE != 0 {
            attrs.push(attribute(
                ATTR_UNDERLINE,
                UNDERLINE_SINGLE,
                offset,
                offset + len,
            ));
        }
        if flags & FORMAT_HIGHLIGHT != 0 {
            attrs.push(attribute(ATTR_FOREGROUND, WHITE, offset, offset + len));
            attrs.push(attribute(ATTR_BACKGROUND, BLACK, offset, offset + len));
        }
        if flags & FORMAT_DONT_COMMIT == 0 {
            dont_commit = false;
        }
        full.push_str(text);
        offset += len;
    }
    let cursor = if byte_cursor >= 0 {
        let end = (byte_cursor as usize).min(full.len());
        // A cursor inside a character would be a fcitx5 bug; round down.
        let end = (0..=end)
            .rev()
            .find(|i| full.is_char_boundary(*i))
            .unwrap_or(0);
        full[..end].chars().count() as u32
    } else {
        0
    };
    Preedit {
        text: text_struct(&full, attrs),
        cursor,
        visible: offset != 0,
        mode: if dont_commit { 0 } else { 1 },
    }
}

/// Pulls the string out of an `IBusText` received as a `v` argument.
pub fn text_of(value: &Value<'_>) -> Option<String> {
    let inner = match value {
        Value::Value(inner) => inner.as_ref(),
        other => other,
    };
    let Value::Structure(s) = inner else {
        return None;
    };
    structure_text(s)
}

fn structure_text(s: &Structure<'_>) -> Option<String> {
    match s.fields().get(2)? {
        Value::Str(text) => Some(text.to_string()),
        _ => None,
    }
}

/// IBus capability bits (`IBusCapabilite`).
pub mod ibus_cap {
    pub const PREEDIT_TEXT: u32 = 1 << 0;
    pub const SURROUNDING_TEXT: u32 = 1 << 5;
}

/// fcitx5 `CapabilityFlag` bits.
pub mod fcitx_cap {
    pub const PREEDIT: u64 = 1 << 1;
    pub const FORMATTED_PREEDIT: u64 = 1 << 4;
    pub const SURROUNDING_TEXT: u64 = 1 << 6;
    pub const RELATIVE_RECT: u64 = 1 << 24;
}

/// fcitx5 flags that describe what kind of field has focus.
pub mod fcitx_content {
    pub const PASSWORD: u64 = 1 << 3;
    pub const EMAIL: u64 = 1 << 7;
    pub const DIGIT: u64 = 1 << 8;
    pub const UPPERCASE: u64 = 1 << 9;
    pub const LOWERCASE: u64 = 1 << 10;
    pub const URL: u64 = 1 << 12;
    pub const DIALABLE: u64 = 1 << 13;
    pub const NUMBER: u64 = 1 << 14;
    pub const NO_ON_SCREEN_KEYBOARD: u64 = 1 << 15;
    pub const SPELL_CHECK: u64 = 1 << 16;
    pub const NO_SPELL_CHECK: u64 = 1 << 17;
    pub const WORD_COMPLETION: u64 = 1 << 18;
    pub const UPPERCASE_WORDS: u64 = 1 << 19;
    pub const UPPERCASE_SENTENCES: u64 = 1 << 20;
    pub const ALPHA: u64 = 1 << 21;
    pub const NAME: u64 = 1 << 22;
}

/// Maps GTK's input purpose and hints, which IBus passes through as the
/// `ContentType` property, to fcitx5 flags. Passwords matter most: fcitx5
/// then keeps input methods from recording what is typed.
pub fn content_type_flags(purpose: u32, hints: u32) -> u64 {
    use fcitx_content::*;
    let mut flags = match purpose {
        1 => ALPHA,
        2 => DIGIT,
        3 => NUMBER,
        4 => DIALABLE,
        5 => URL,
        6 => EMAIL,
        7 => NAME,
        8 => PASSWORD,
        9 => PASSWORD | DIGIT,
        _ => 0,
    };
    for (hint, flag) in [
        (1 << 0, SPELL_CHECK),
        (1 << 1, NO_SPELL_CHECK),
        (1 << 2, WORD_COMPLETION),
        (1 << 3, LOWERCASE),
        (1 << 4, UPPERCASE),
        (1 << 5, UPPERCASE_WORDS),
        (1 << 6, UPPERCASE_SENTENCES),
        (1 << 7, NO_ON_SCREEN_KEYBOARD),
    ] {
        if hints & hint != 0 {
            flags |= flag;
        }
    }
    flags
}

/// Maps the capability bits an IBus client sets to fcitx5's flags. `relative`
/// keeps the RelativeRect bit the client chose with its last cursor update.
pub fn capabilities(ibus: u32, relative: bool) -> u64 {
    let mut flags = 0;
    if ibus & ibus_cap::PREEDIT_TEXT != 0 {
        flags |= fcitx_cap::PREEDIT | fcitx_cap::FORMATTED_PREEDIT;
    }
    if ibus & ibus_cap::SURROUNDING_TEXT != 0 {
        flags |= fcitx_cap::SURROUNDING_TEXT;
    }
    if relative {
        flags |= fcitx_cap::RELATIVE_RECT;
    }
    flags
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{serialized::Context, to_bytes, Type, LE};

    fn signature_of(v: &Value<'_>) -> String {
        v.value_signature().to_string()
    }

    #[test]
    fn text_has_ibus_signature() {
        // Sent as a `v` argument, the variant then wraps exactly `(sa{sv}sv)`.
        let v = ibus_text("hi");
        assert_eq!(signature_of(&v), "(sa{sv}sv)");
        let Value::Structure(s) = &v else {
            panic!("not a struct")
        };
        // Field 3 is the attribute list, itself a variant around `(sa{sv}av)`.
        let Value::Value(list) = &s.fields()[3] else {
            panic!("attrs not a variant")
        };
        assert_eq!(signature_of(list), "(sa{sv}av)");
    }

    #[test]
    fn attribute_list_is_an_array_of_variants_even_when_empty() {
        let v = ibus_text("");
        let Value::Structure(s) = &v else { panic!() };
        let Value::Value(list) = &s.fields()[3] else {
            panic!()
        };
        let Value::Structure(l) = list.as_ref() else {
            panic!()
        };
        assert_eq!(signature_of(&l.fields()[2]), "av");
    }

    #[test]
    fn body_is_one_variant_deep() {
        // A signal `CommitText(v)` body serializes as `(v)` holding the struct
        // directly; an extra variant level would break GNOME's parser.
        let body = (ibus_text("x"),);
        let sig = <(Value<'_>,) as Type>::SIGNATURE.to_string();
        assert_eq!(sig, "(v)");
        let ctx = Context::new_dbus(LE, 0);
        let bytes = to_bytes(ctx, &body).expect("serializes");
        // Variant signature byte string for `(sa{sv}sv)` follows the length
        // byte: 0x0a "(sa{sv}sv)".
        let needle = b"(sa{sv}sv)\0";
        assert!(
            bytes.windows(needle.len()).any(|w| w == needle),
            "inner signature missing"
        );
        // And no `v` signature directly in front of it, which would mean a
        // second variant.
        let double = b"\x01v\0";
        assert!(!bytes.windows(double.len()).any(|w| w == double));
    }

    #[test]
    fn text_round_trips() {
        let v = ibus_text("á日本");
        assert_eq!(text_of(&v).as_deref(), Some("á日本"));
    }

    #[test]
    fn text_serializes_on_the_wire() {
        let v = ibus_text("x");
        let ctx = Context::new_dbus(LE, 0);
        let bytes = to_bytes(ctx, &v).expect("serializes");
        assert!(!bytes.is_empty());
        let _ = <Value<'_> as Type>::SIGNATURE;
    }

    #[test]
    fn preedit_marks_underline_and_converts_cursor() {
        // "ab" underlined then "é" plain; cursor after "ab" is byte 2.
        let p = preedit(&[("ab".into(), FORMAT_UNDERLINE), ("é".into(), 0)], 2);
        assert_eq!(p.cursor, 2);
        assert!(p.visible);
        assert_eq!(p.mode, 1);
        assert_eq!(text_of(&p.text).as_deref(), Some("abé"));
    }

    #[test]
    fn preedit_cursor_counts_characters_not_bytes() {
        // Cursor after the 3-byte "日": byte 3, character 1.
        let p = preedit(&[("日本".into(), 0)], 3);
        assert_eq!(p.cursor, 1);
    }

    #[test]
    fn preedit_cursor_inside_a_character_rounds_down() {
        let p = preedit(&[("日".into(), 0)], 2);
        assert_eq!(p.cursor, 0);
    }

    #[test]
    fn empty_preedit_is_hidden() {
        let p = preedit(&[], -1);
        assert!(!p.visible);
        assert_eq!(p.cursor, 0);
    }

    #[test]
    fn dont_commit_only_when_every_segment_has_it() {
        let all = preedit(
            &[
                ("a".into(), FORMAT_DONT_COMMIT),
                ("b".into(), FORMAT_DONT_COMMIT),
            ],
            0,
        );
        assert_eq!(all.mode, 0);
        let some = preedit(&[("a".into(), FORMAT_DONT_COMMIT), ("b".into(), 0)], 0);
        assert_eq!(some.mode, 1);
    }

    #[test]
    fn highlight_adds_foreground_and_background() {
        let p = preedit(&[("a".into(), FORMAT_HIGHLIGHT)], 0);
        // text_struct -> attr list -> two attributes
        let Value::Structure(s) = &p.text else {
            panic!()
        };
        let Value::Value(list) = &s.fields()[3] else {
            panic!()
        };
        let Value::Structure(l) = list.as_ref() else {
            panic!()
        };
        let Value::Array(a) = &l.fields()[2] else {
            panic!()
        };
        assert_eq!(a.len(), 2);
    }

    #[test]
    fn password_fields_are_flagged() {
        use fcitx_content::*;
        assert_eq!(content_type_flags(8, 0), PASSWORD);
        assert_eq!(content_type_flags(9, 0), PASSWORD | DIGIT);
        assert_eq!(content_type_flags(0, 0), 0);
        assert_eq!(content_type_flags(0, 1 | 4), SPELL_CHECK | WORD_COMPLETION);
        assert_eq!(content_type_flags(6, 1 << 7), EMAIL | NO_ON_SCREEN_KEYBOARD);
    }

    #[test]
    fn capabilities_map() {
        assert_eq!(capabilities(0, false), 0);
        assert_eq!(
            capabilities(ibus_cap::PREEDIT_TEXT, false),
            fcitx_cap::PREEDIT | fcitx_cap::FORMATTED_PREEDIT
        );
        assert_eq!(
            capabilities(ibus_cap::SURROUNDING_TEXT, true),
            fcitx_cap::SURROUNDING_TEXT | fcitx_cap::RELATIVE_RECT
        );
    }
}
