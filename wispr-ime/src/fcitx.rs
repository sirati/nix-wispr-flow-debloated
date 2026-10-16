//! Client side of fcitx5's D-Bus frontend (`org.fcitx.Fcitx.InputMethod1` and
//! `InputContext1`), the same API fcitx5's own GTK and Qt modules use.

use zbus::message::Type as MessageType;
use zbus::zvariant::{DynamicType, OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, Message};

use crate::wire::Segment;

pub const SERVICE: &str = "org.fcitx.Fcitx5";
const IM_PATH: &str = "/org/freedesktop/portal/inputmethod";
const IM_IFACE: &str = "org.fcitx.Fcitx.InputMethod1";
pub const IC_IFACE: &str = "org.fcitx.Fcitx.InputContext1";

/// Something fcitx5 wants the client to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Commit(String),
    Preedit {
        segments: Vec<Segment>,
        cursor: i32,
    },
    Forward {
        sym: u32,
        states: u32,
        release: bool,
    },
    Delete {
        offset: i32,
        size: u32,
    },
}

/// Creates an input context and returns its object path.
pub async fn create_ic(conn: &Connection, program: &str) -> zbus::Result<OwnedObjectPath> {
    let args = vec![("program".to_string(), program.to_string())];
    let reply = conn
        .call_method(
            Some(SERVICE),
            IM_PATH,
            Some(IM_IFACE),
            "CreateInputContext",
            &(args,),
        )
        .await?;
    let (path, _uuid): (OwnedObjectPath, Vec<u8>) = reply.body().deserialize()?;
    Ok(path)
}

/// Calls a method on an input context and waits for the (empty) reply.
pub async fn call<B>(conn: &Connection, path: &str, member: &str, body: &B) -> zbus::Result<()>
where
    B: serde::Serialize + DynamicType,
{
    conn.call_method(Some(SERVICE), path, Some(IC_IFACE), member, body)
        .await?;
    Ok(())
}

/// Sends a key through `ProcessKeyEventBatch`, which returns the commits and
/// preedit updates the key caused inside the reply, in order, instead of as
/// separate signals that could arrive on either side of it.
pub async fn process_key(
    conn: &Connection,
    path: &str,
    keyval: u32,
    keycode: u32,
    states: u32,
    release: bool,
) -> zbus::Result<(Vec<Event>, bool)> {
    let reply = conn
        .call_method(
            Some(SERVICE),
            path,
            Some(IC_IFACE),
            "ProcessKeyEventBatch",
            &(keyval, keycode, states, release, 0u32),
        )
        .await?;
    let (raw, handled): (Vec<(u32, OwnedValue)>, bool) = reply.body().deserialize()?;
    let events = raw
        .into_iter()
        .filter_map(|(kind, value)| decode_batched(kind, &value))
        .collect();
    Ok((events, handled))
}

/// `BATCHED_*` event types from fcitx5's dbusfrontend.
fn decode_batched(kind: u32, value: &Value<'_>) -> Option<Event> {
    match kind {
        0 => match value {
            Value::Str(s) => Some(Event::Commit(s.to_string())),
            _ => None,
        },
        1 => {
            let Value::Structure(st) = value else {
                return None;
            };
            let [segments, cursor] = st.fields() else {
                return None;
            };
            Some(Event::Preedit {
                segments: segments_of(segments)?,
                cursor: i32_of(cursor)?,
            })
        }
        2 => {
            let Value::Structure(st) = value else {
                return None;
            };
            let [sym, states, release] = st.fields() else {
                return None;
            };
            Some(Event::Forward {
                sym: u32_of(sym)?,
                states: u32_of(states)?,
                release: bool_of(release)?,
            })
        }
        3 => {
            let Value::Structure(st) = value else {
                return None;
            };
            let [offset, size] = st.fields() else {
                return None;
            };
            Some(Event::Delete {
                offset: i32_of(offset)?,
                size: u32_of(size)?,
            })
        }
        _ => None,
    }
}

fn unwrap<'a>(v: &'a Value<'a>) -> &'a Value<'a> {
    match v {
        Value::Value(inner) => inner.as_ref(),
        other => other,
    }
}

fn i32_of(v: &Value<'_>) -> Option<i32> {
    match unwrap(v) {
        Value::I32(n) => Some(*n),
        _ => None,
    }
}

fn u32_of(v: &Value<'_>) -> Option<u32> {
    match unwrap(v) {
        Value::U32(n) => Some(*n),
        _ => None,
    }
}

fn bool_of(v: &Value<'_>) -> Option<bool> {
    match unwrap(v) {
        Value::Bool(b) => Some(*b),
        _ => None,
    }
}

fn segments_of(v: &Value<'_>) -> Option<Vec<Segment>> {
    let Value::Array(array) = unwrap(v) else {
        return None;
    };
    array
        .iter()
        .map(|item| {
            let Value::Structure(st) = unwrap(item) else {
                return None;
            };
            let [text, flags] = st.fields() else {
                return None;
            };
            let Value::Str(text) = unwrap(text) else {
                return None;
            };
            Some((text.to_string(), i32_of(flags)?))
        })
        .collect()
}

/// Reads one of fcitx5's stand-alone signals, which it sends to the client's
/// unique name for changes that do not come from a key press (for example a
/// candidate picked with the mouse). Returns the input context's path.
pub fn decode_signal(msg: &Message) -> Option<(String, Event)> {
    let header = msg.header();
    if header.message_type() != MessageType::Signal || header.interface()?.as_str() != IC_IFACE {
        return None;
    }
    let path = header.path()?.to_string();
    let body = msg.body();
    let event = match header.member()?.as_str() {
        "CommitString" => Event::Commit(body.deserialize::<(String,)>().ok()?.0),
        "UpdateFormattedPreedit" => {
            let (segments, cursor): (Vec<Segment>, i32) = body.deserialize().ok()?;
            Event::Preedit { segments, cursor }
        }
        "ForwardKey" => {
            let (sym, states, release): (u32, u32, bool) = body.deserialize().ok()?;
            Event::Forward {
                sym,
                states,
                release,
            }
        }
        "DeleteSurroundingText" => {
            let (offset, size): (i32, u32) = body.deserialize().ok()?;
            Event::Delete { offset, size }
        }
        _ => return None,
    };
    Some((path, event))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{Array, Signature, StructureBuilder};

    #[test]
    fn commit_payload() {
        let v = Value::from("é");
        assert_eq!(decode_batched(0, &v), Some(Event::Commit("é".into())));
    }

    #[test]
    fn preedit_payload() {
        let mut segs = Array::new(&Signature::Structure(
            zbus::zvariant::signature::Fields::Static {
                fields: &[&Signature::Str, &Signature::I32],
            },
        ));
        let seg = StructureBuilder::new()
            .add_field("ab".to_string())
            .add_field(8i32)
            .build()
            .unwrap();
        segs.append(Value::Structure(seg)).unwrap();
        let st = StructureBuilder::new()
            .add_field(segs)
            .add_field(2i32)
            .build()
            .unwrap();
        // Struct fields typed `v` arrive wrapped; the decoder looks through.
        let ev = decode_batched(1, &Value::Structure(st));
        assert_eq!(
            ev,
            Some(Event::Preedit {
                segments: vec![("ab".into(), 8)],
                cursor: 2
            })
        );
    }

    #[test]
    fn forward_and_delete_payloads() {
        let f = StructureBuilder::new()
            .add_field(0x61u32)
            .add_field(4u32)
            .add_field(true)
            .build()
            .unwrap();
        assert_eq!(
            decode_batched(2, &Value::Structure(f)),
            Some(Event::Forward {
                sym: 0x61,
                states: 4,
                release: true
            })
        );
        let d = StructureBuilder::new()
            .add_field(-1i32)
            .add_field(1u32)
            .build()
            .unwrap();
        assert_eq!(
            decode_batched(3, &Value::Structure(d)),
            Some(Event::Delete {
                offset: -1,
                size: 1
            })
        );
    }

    #[test]
    fn unknown_kind_is_ignored() {
        assert_eq!(decode_batched(99, &Value::from(1u32)), None);
        assert_eq!(decode_batched(0, &Value::from(1u32)), None);
    }
}
