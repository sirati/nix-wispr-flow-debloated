//! Just enough D-Bus for one call that takes a string and answers a
//! boolean (wispr-ime's Commit): connect to the session bus, authenticate
//! as this user, say Hello, call, read the reply. Written out here because
//! a D-Bus library brings an async runtime and some sixty crates for it.
//! The tests check it against a real dbus-daemon.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::time::Duration;

const METHOD_CALL: u8 = 1;
const METHOD_RETURN: u8 = 2;
const ERROR: u8 = 3;

// Header fields.
const PATH: u8 = 1;
const INTERFACE: u8 = 2;
const MEMBER: u8 = 3;
const ERROR_NAME: u8 = 4;
const REPLY_SERIAL: u8 = 5;
const DESTINATION: u8 = 6;
const SIGNATURE: u8 = 8;

/// The bus daemon refuses messages above 128 MiB; nothing we read is close.
const MAX_MESSAGE: usize = 1 << 20;

pub struct Call<'a> {
    pub destination: &'a str,
    pub path: &'a str,
    pub interface: &'a str,
    pub member: &'a str,
}

fn pad(buf: &mut Vec<u8>, align: usize) {
    while !buf.len().is_multiple_of(align) {
        buf.push(0);
    }
}

fn put_u32(buf: &mut Vec<u8>, v: u32) {
    pad(buf, 4);
    buf.extend_from_slice(&v.to_le_bytes());
}

fn put_str(buf: &mut Vec<u8>, s: &str) {
    put_u32(buf, s.len() as u32);
    buf.extend_from_slice(s.as_bytes());
    buf.push(0);
}

/// A header field holding a string-like value of `sig` (s, o or g).
fn put_field(buf: &mut Vec<u8>, code: u8, sig: u8, value: &str) {
    pad(buf, 8);
    buf.extend_from_slice(&[code, 1, sig, 0]);
    if sig == b'g' {
        buf.push(value.len() as u8);
        buf.extend_from_slice(value.as_bytes());
        buf.push(0);
    } else {
        put_str(buf, value);
    }
}

/// A method call, little-endian; `arg` is its one string argument.
fn method_call(serial: u32, call: &Call, arg: Option<&str>) -> Vec<u8> {
    let mut body = Vec::new();
    if let Some(arg) = arg {
        put_str(&mut body, arg);
    }
    let mut msg = vec![b'l', METHOD_CALL, 0, 1];
    msg.extend_from_slice(&(body.len() as u32).to_le_bytes());
    msg.extend_from_slice(&serial.to_le_bytes());
    // The fields' length, filled in below.
    msg.extend_from_slice(&[0; 4]);
    put_field(&mut msg, PATH, b'o', call.path);
    put_field(&mut msg, INTERFACE, b's', call.interface);
    put_field(&mut msg, MEMBER, b's', call.member);
    put_field(&mut msg, DESTINATION, b's', call.destination);
    if arg.is_some() {
        put_field(&mut msg, SIGNATURE, b'g', "s");
    }
    let fields = (msg.len() - 16) as u32;
    msg[12..16].copy_from_slice(&fields.to_le_bytes());
    pad(&mut msg, 8);
    msg.extend_from_slice(&body);
    msg
}

/// A message read back, with what we need of it.
#[derive(Debug, Default, PartialEq)]
struct Reply {
    kind: u8,
    reply_serial: Option<u32>,
    error_name: Option<String>,
    signature: String,
    /// The body's first value as a boolean or a string, if it is one.
    boolean: Option<bool>,
    text: Option<String>,
}

/// Reads values at offsets counted from the message start, which is what
/// D-Bus aligns to.
struct Cursor<'a> {
    buf: &'a [u8],
    at: usize,
    little: bool,
}

impl Cursor<'_> {
    fn align(&mut self, n: usize) {
        self.at = self.at.div_ceil(n) * n;
    }

    fn bytes(&mut self, n: usize) -> Result<&[u8], String> {
        let end = self.at.checked_add(n).filter(|&e| e <= self.buf.len());
        let end = end.ok_or("D-Bus: message cut short")?;
        let b = &self.buf[self.at..end];
        self.at = end;
        Ok(b)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.bytes(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, String> {
        self.align(4);
        let b: [u8; 4] = self.bytes(4)?.try_into().expect("four bytes");
        Ok(if self.little {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    }

    fn str(&mut self) -> Result<String, String> {
        let len = self.u32()? as usize;
        let s = String::from_utf8_lossy(self.bytes(len)?).into_owned();
        self.bytes(1)?;
        Ok(s)
    }

    fn sig(&mut self) -> Result<String, String> {
        let len = usize::from(self.u8()?);
        let s = String::from_utf8_lossy(self.bytes(len)?).into_owned();
        self.bytes(1)?;
        Ok(s)
    }
}

/// The length of the message whose first 16 bytes are `head`.
fn message_len(head: &[u8; 16]) -> Result<usize, String> {
    let u32_at = |i: usize| {
        let b: [u8; 4] = head[i..i + 4].try_into().expect("four bytes");
        match head[0] {
            b'l' => Ok(u32::from_le_bytes(b) as usize),
            b'B' => Ok(u32::from_be_bytes(b) as usize),
            _ => Err("D-Bus: not a message".to_string()),
        }
    };
    let (body, fields) = (u32_at(4)?, u32_at(12)?);
    let len = (16 + fields).div_ceil(8) * 8 + body;
    if len > MAX_MESSAGE {
        return Err("D-Bus: message too long".into());
    }
    Ok(len)
}

fn parse(buf: &[u8]) -> Result<Reply, String> {
    let mut c = Cursor {
        buf,
        at: 0,
        little: buf.first() == Some(&b'l'),
    };
    let head = c.bytes(4)?;
    let mut reply = Reply {
        kind: head[1],
        ..Reply::default()
    };
    let _body_len = c.u32()?;
    let _serial = c.u32()?;
    let fields_end = c.u32()? as usize + 16;
    while c.at < fields_end {
        c.align(8);
        let code = c.u8()?;
        let sig = c.sig()?;
        match (code, sig.as_str()) {
            (REPLY_SERIAL, "u") => reply.reply_serial = Some(c.u32()?),
            (ERROR_NAME, "s") => reply.error_name = Some(c.str()?),
            (SIGNATURE, "g") => reply.signature = c.sig()?,
            (_, "s" | "o") => {
                c.str()?;
            }
            (_, "g") => {
                c.sig()?;
            }
            (_, "u") => {
                c.u32()?;
            }
            _ => return Err(format!("D-Bus: header field {code} of type {sig}")),
        }
    }
    c.align(8);
    if reply.signature.starts_with('b') {
        reply.boolean = Some(c.u32()? != 0);
    } else if reply.signature.starts_with('s') {
        reply.text = Some(c.str()?);
    }
    Ok(reply)
}

/// The session bus's socket, from DBUS_SESSION_BUS_ADDRESS or the usual
/// $XDG_RUNTIME_DIR/bus.
fn connect_session() -> Result<UnixStream, String> {
    if let Ok(address) = std::env::var("DBUS_SESSION_BUS_ADDRESS") {
        return connect(&address);
    }
    let dir = std::env::var("XDG_RUNTIME_DIR").map_err(|_| "D-Bus: no session bus")?;
    UnixStream::connect(format!("{dir}/bus")).map_err(|e| format!("D-Bus: {e}"))
}

/// Undoes D-Bus address escaping (%2f and the like).
fn unescape(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let hex = b.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok());
        match (b[i], hex.and_then(|h| u8::from_str_radix(h, 16).ok())) {
            (b'%', Some(v)) => {
                out.push(v);
                i += 3;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The first of `address`'s unix:path= or unix:abstract= sockets that
/// accepts.
fn connect(address: &str) -> Result<UnixStream, String> {
    let mut last = "D-Bus: no unix socket in the bus address".to_string();
    for entry in address.split(';') {
        let Some(params) = entry.strip_prefix("unix:") else {
            continue;
        };
        for kv in params.split(',') {
            let stream = match kv.split_once('=') {
                Some(("path", p)) => UnixStream::connect(unescape(p)),
                Some(("abstract", name)) => {
                    use std::os::linux::net::SocketAddrExt;
                    std::os::unix::net::SocketAddr::from_abstract_name(unescape(name))
                        .and_then(|a| UnixStream::connect_addr(&a))
                }
                _ => continue,
            };
            match stream {
                Ok(s) => return Ok(s),
                Err(e) => last = format!("D-Bus: {e}"),
            }
        }
    }
    Err(last)
}

/// Calls `call` with one string on the session bus and returns its boolean
/// answer. `timeout` bounds every read and write.
pub fn call_bool(call: &Call, arg: &str, timeout: Duration) -> Result<bool, String> {
    call_bool_on(connect_session()?, call, arg, timeout)
}

fn call_bool_on(
    stream: UnixStream,
    call: &Call,
    arg: &str,
    timeout: Duration,
) -> Result<bool, String> {
    let io = |e: std::io::Error| format!("D-Bus: {e}");
    stream.set_read_timeout(Some(timeout)).map_err(io)?;
    stream.set_write_timeout(Some(timeout)).map_err(io)?;
    let mut writer = stream.try_clone().map_err(io)?;
    let mut reader = BufReader::new(stream);
    // EXTERNAL: the bus checks the socket's peer credentials against the uid
    // we name, in hex-encoded decimal. /proc/self is owned by our uid.
    let uid = std::fs::metadata("/proc/self").map_err(io)?.uid();
    let hex: String = uid.to_string().bytes().map(|b| format!("{b:02x}")).collect();
    writer
        .write_all(format!("\0AUTH EXTERNAL {hex}\r\n").as_bytes())
        .map_err(io)?;
    let mut line = String::new();
    reader.read_line(&mut line).map_err(io)?;
    if !line.starts_with("OK ") {
        return Err(format!("D-Bus: authentication refused: {}", line.trim()));
    }
    let hello = Call {
        destination: "org.freedesktop.DBus",
        path: "/org/freedesktop/DBus",
        interface: "org.freedesktop.DBus",
        member: "Hello",
    };
    let mut out = b"BEGIN\r\n".to_vec();
    out.extend(method_call(1, &hello, None));
    out.extend(method_call(2, call, Some(arg)));
    writer.write_all(&out).map_err(io)?;
    // Our name being assigned, the Hello reply, then ours.
    loop {
        let mut head = [0u8; 16];
        reader.read_exact(&mut head).map_err(io)?;
        let mut msg = head.to_vec();
        msg.resize(message_len(&head)?, 0);
        reader.read_exact(&mut msg[16..]).map_err(io)?;
        let reply = parse(&msg)?;
        if reply.reply_serial != Some(2) {
            continue;
        }
        return match reply.kind {
            METHOD_RETURN => reply
                .boolean
                .ok_or_else(|| format!("D-Bus: answer of type {}", reply.signature)),
            ERROR => Err(format!(
                "{}{}",
                reply.error_name.unwrap_or_default(),
                reply.text.map(|t| format!(": {t}")).unwrap_or_default()
            )),
            _ => Err("D-Bus: unexpected message".into()),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    const NAME_HAS_OWNER: Call = Call {
        destination: "org.freedesktop.DBus",
        path: "/org/freedesktop/DBus",
        interface: "org.freedesktop.DBus",
        member: "NameHasOwner",
    };

    #[test]
    fn a_call_is_marshalled_as_the_spec_says() {
        let msg = method_call(7, &NAME_HAS_OWNER, Some("a.b"));
        assert_eq!(&msg[..4], b"l\x01\x00\x01");
        // Body: the string's length, "a.b", NUL.
        assert_eq!(u32::from_le_bytes(msg[4..8].try_into().unwrap()), 8);
        assert_eq!(u32::from_le_bytes(msg[8..12].try_into().unwrap()), 7);
        assert_eq!(&msg[msg.len() - 8..], b"\x03\x00\x00\x00a.b\x00");
        assert_eq!((msg.len() - 8) % 8, 0, "body starts 8-aligned");
        // It reads back with our own parser.
        let r = parse(&msg).unwrap();
        assert_eq!(r.kind, METHOD_CALL);
        assert_eq!(r.signature, "s");
        assert_eq!(r.text.as_deref(), Some("a.b"));
        assert_eq!(message_len(&msg[..16].try_into().unwrap()), Ok(msg.len()));
    }

    #[test]
    fn replies_parse_in_either_byte_order() {
        // A big-endian method return to serial 2 carrying `true`.
        let mut msg = vec![b'B', METHOD_RETURN, 0, 1, 0, 0, 0, 4, 0, 0, 0, 9];
        let mut fields = vec![REPLY_SERIAL, 1, b'u', 0, 0, 0, 0, 2];
        fields.extend([SIGNATURE, 1, b'g', 0, 1, b'b', 0]);
        msg.extend((fields.len() as u32).to_be_bytes());
        msg.extend(&fields);
        while !msg.len().is_multiple_of(8) {
            msg.push(0);
        }
        msg.extend(1u32.to_be_bytes());
        let r = parse(&msg).unwrap();
        assert_eq!(r.reply_serial, Some(2));
        assert_eq!(r.boolean, Some(true));
        assert!(parse(&msg[..20]).is_err());
    }

    #[test]
    fn addresses_unescape() {
        assert_eq!(unescape("/run/user/1000/bus"), "/run/user/1000/bus");
        assert_eq!(unescape("%2frun%2Fx"), "/run/x");
        assert_eq!(unescape("50%"), "50%");
    }

    /// A private bus, if dbus-daemon is on PATH.
    fn bus() -> Option<(std::process::Child, String)> {
        let config = std::env::var_os("PATH").and_then(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join("../share/dbus-1/session.conf"))
                .find(|c| c.is_file())
        });
        let Some(config) = config else {
            eprintln!("skipped: no dbus-daemon");
            return None;
        };
        let mut child = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--print-address"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut address = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        Some((child, address.trim().to_string()))
    }

    #[test]
    fn calls_a_real_bus() {
        let Some((mut daemon, address)) = bus() else {
            return;
        };
        let t = Duration::from_secs(5);
        let ask = |name: &str| call_bool_on(connect(&address).unwrap(), &NAME_HAS_OWNER, name, t);
        assert_eq!(ask("org.freedesktop.DBus"), Ok(true));
        assert_eq!(ask("org.wisprime.WisprIme"), Ok(false));
        let nobody = Call {
            destination: "org.wisprime.WisprIme",
            path: "/org/wisprime/WisprIme",
            interface: "org.wisprime.WisprIme1",
            member: "Commit",
        };
        let e = call_bool_on(connect(&address).unwrap(), &nobody, "text", t).unwrap_err();
        assert!(e.contains("ServiceUnknown"), "{e}");
        let _ = daemon.kill();
        let _ = daemon.wait();
    }
}
