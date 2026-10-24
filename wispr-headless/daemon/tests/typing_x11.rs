//! Types into a scratch X server (Xvfb) through XTEST, as on an X11 desktop,
//! and checks what a focused X window receives: in the server's default
//! layout, where some characters have to borrow a keycode, and in a German
//! one, with AltGr and dead keys. Skipped without Xvfb (and setxkbmap for
//! the German layout) on PATH.
#![cfg(target_os = "linux")]

use std::io::Write;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ConnectionExt as _, CreateWindowAux, EventMask, InputFocus, KeyButMask, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(program).is_file()))
}

struct Xvfb {
    child: Child,
    display: String,
    /// One server at a time: the tests type through the same XTEST.
    _alone: MutexGuard<'static, ()>,
}

static ALONE: Mutex<()> = Mutex::new(());
static NEXT: AtomicUsize = AtomicUsize::new(0);

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A child that is killed and reaped however the test ends.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn xvfb() -> Option<(Xvfb, RustConnection, usize)> {
    if !on_path("Xvfb") {
        eprintln!("skipped: no Xvfb");
        return None;
    }
    let alone = ALONE.lock().unwrap_or_else(|e| e.into_inner());
    for _ in 0..20 {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let number = 140 + (std::process::id() as usize + n * 37) % 800;
        if Path::new(&format!("/tmp/.X{number}-lock")).exists() {
            continue;
        }
        let display = format!(":{number}");
        let child = Command::new("Xvfb")
            .args([display.as_str(), "-nolisten", "tcp", "-screen", "0", "640x480x24"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut child = child;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            // Taken after all: try another number.
            if child.try_wait().ok().flatten().is_some() {
                break;
            }
            if let Ok((conn, screen)) = x11rb::connect(Some(&display)) {
                let x = Xvfb {
                    child,
                    display,
                    _alone: alone,
                };
                return Some((x, conn, screen));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    panic!("Xvfb did not start");
}

/// The server's core mapping, to read keys back as a client would.
struct Mapping {
    min: u8,
    per: usize,
    keysyms: Vec<u32>,
}

impl Mapping {
    fn fetch(conn: &RustConnection) -> Mapping {
        let setup = conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let reply = conn
            .get_keyboard_mapping(min, max - min + 1)
            .unwrap()
            .reply()
            .unwrap();
        Mapping {
            min,
            per: usize::from(reply.keysyms_per_keycode),
            keysyms: reply.keysyms,
        }
    }

    /// What a key press with `state` types. Level 3 is Mod5, as in the
    /// layouts here.
    fn char_of(&self, keycode: u8, state: KeyButMask) -> Option<char> {
        let start = usize::from(keycode - self.min) * self.per;
        let row = &self.keysyms[start..start + self.per];
        let shift = state.contains(KeyButMask::SHIFT);
        let col = if state.contains(KeyButMask::MOD5) { 4 } else { 0 } + usize::from(shift);
        let mut keysym = row.get(col).copied().unwrap_or(0);
        if keysym == 0 && shift {
            keysym = row[col - 1];
        }
        match keysym {
            0xff0d => Some('\n'),
            0xff09 => Some('\t'),
            0x20..=0x7e | 0xa0..=0xff => char::from_u32(keysym),
            0x20ac => Some('€'),
            0x0100_0100.. => char::from_u32(keysym - 0x0100_0000),
            _ => None,
        }
    }
}

/// Runs `wispr-headless --type-test` against a focused window and returns
/// what the window was typed.
fn typed(conn: &RustConnection, screen: usize, display: &str, text: &str) -> String {
    let root = conn.setup().roots[screen].root;
    let win = conn.generate_id().unwrap();
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        win,
        root,
        0,
        0,
        200,
        100,
        0,
        WindowClass::INPUT_OUTPUT,
        x11rb::COPY_FROM_PARENT,
        &CreateWindowAux::new().event_mask(EventMask::KEY_PRESS),
    )
    .unwrap();
    conn.map_window(win).unwrap();
    conn.set_input_focus(InputFocus::PARENT, win, x11rb::CURRENT_TIME)
        .unwrap();
    conn.get_input_focus().unwrap().reply().unwrap();
    let mut mapping = Mapping::fetch(conn);

    let child = Command::new(env!("CARGO_BIN_EXE_wispr-headless"))
        .args(["--type-test", "--type-with", "keys"])
        .env("DISPLAY", display)
        .env_remove("WAYLAND_DISPLAY")
        .env("WISPR_HEADLESS_TEST_TYPE_DELAY_MS", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .expect("wispr-headless starts");
    let mut child = Reaped(child);
    child
        .0
        .stdin
        .take()
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
    let mut out = String::new();
    let mut done: Option<Instant> = None;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        while let Some(event) = conn.poll_for_event().unwrap() {
            match event {
                // A borrowed keycode: read it as the client after us would.
                Event::MappingNotify(_) => mapping = Mapping::fetch(conn),
                Event::KeyPress(e) => out.extend(mapping.char_of(e.detail, e.state)),
                _ => {}
            }
        }
        if done.is_none() {
            if let Some(status) = child.0.try_wait().unwrap() {
                assert!(status.success(), "wispr-headless failed: {status}");
                done = Some(Instant::now());
            }
        }
        if done.is_some_and(|t| t.elapsed() > Duration::from_millis(300)) {
            break;
        }
        assert!(Instant::now() < deadline, "typing took too long");
        std::thread::sleep(Duration::from_millis(1));
    }
    conn.destroy_window(win).unwrap();
    conn.flush().unwrap();
    out
}

#[test]
fn xtest_types_any_character_in_the_default_layout() {
    let Some((x, conn, screen)) = xvfb() else { return };
    // ü, ß, €, ą and 😀 are not in the US layout and borrow a keycode.
    let text = "Hello, World! (1+1=2) ~`'\"\\|<>?\nGrüße € ą 😀\tend";
    assert_eq!(typed(&conn, screen, &x.display, text), text);
    // The borrowed keycodes are given back.
    let setup = conn.setup();
    let (min, max) = (setup.min_keycode, setup.max_keycode);
    let reply = conn
        .get_keyboard_mapping(min, max - min + 1)
        .unwrap()
        .reply()
        .unwrap();
    for s in ['ü', 'ß', '€', 'ą', '😀'].map(|c| 0x0100_0000 + u32::from(c)) {
        assert!(!reply.keysyms.contains(&s), "{s:#x} still mapped");
    }
    assert!(!reply.keysyms.contains(&0xfc), "ü still mapped");
}

#[test]
fn xtest_uses_altgr_and_avoids_dead_keys_in_a_german_layout() {
    if !on_path("setxkbmap") {
        eprintln!("skipped: no setxkbmap");
        return;
    }
    let Some((x, conn, screen)) = xvfb() else { return };
    let status = Command::new("setxkbmap")
        .args(["-display", &x.display, "de"])
        .status()
        .unwrap();
    assert!(status.success(), "setxkbmap de");
    // Read the new layout.
    conn.get_input_focus().unwrap().reply().unwrap();
    while conn.poll_for_event().unwrap().is_some() {}
    // z and y swap places; €, @, braces and µ are AltGr; ^ and ` are dead
    // keys here, so they borrow a keycode.
    let text = "zyZY äöü ß € @ {[]} µ ^ ` ~";
    assert_eq!(typed(&conn, screen, &x.display, text), text);
}
