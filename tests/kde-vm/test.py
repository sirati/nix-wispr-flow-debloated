# Test script for tests/kde-vm/default.nix, which defines DAEMON and WL_COPY
# before this. `machine` and `start_all` come from the NixOS test driver.

import shlex
import time

USER = "alice"


def log(*a):
    print("[kde-vm]", *a, flush=True)


def session_env():
    """The Plasma session's environment, as plasmashell has it."""
    pid = machine.succeed(f"pgrep -u {USER} plasmashell | head -n1").strip()
    env = machine.succeed(f"tr '\\0' '\\n' < /proc/{pid}/environ").splitlines()
    keep = (
        "DISPLAY",
        "XAUTHORITY",
        "WAYLAND_DISPLAY",
        "XDG_RUNTIME_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
        "HOME",
        "PATH",
    )
    return " ".join(shlex.quote(e) for e in env if e.split("=", 1)[0] in keep)


def as_alice(cmd, timeout=60):
    return machine.succeed(
        f"runuser -u {USER} -- env {ENV} sh -c {shlex.quote(cmd)}", timeout=timeout
    )


def in_background(cmd, out):
    as_alice(f"{cmd} >{out} 2>&1 &")


def read(path):
    return machine.succeed(f"cat {path} 2>/dev/null || true")


def wait_for_text(path, expected, timeout=30):
    deadline = time.time() + timeout
    got = None
    while time.time() < deadline:
        got = read(path)
        if got == expected:
            return
        time.sleep(0.5)
    raise Exception(f"{path}: expected {expected!r}, got {got!r}")


def type_text(text, method="keys", focused=None):
    if focused:
        machine.succeed(f"test -e {focused}.focused")
    out = as_alice(
        f"printf %s {shlex.quote(text)} | WISPR_HEADLESS_TEST_TYPE_DELAY_MS=0 "
        f"{DAEMON} --type-test --type-with {method} --wl-copy {WL_COPY} 2>&1"
    )
    log("type-test:", out.strip())


# ---------------------------------------------------------------- session
start_all()
machine.wait_until_succeeds(f"pgrep -u {USER} plasmashell", timeout=300)
machine.wait_until_succeeds(f"pgrep -u {USER} Xwayland", timeout=120)
time.sleep(10)
ENV = session_env()
log("session:", ENV)
assert "WAYLAND_DISPLAY=" in ENV and "DISPLAY=" in ENV, ENV

# ---------------------------------------------------------------- the shortcut
HOTKEY_LOG = "/tmp/hotkey.log"
seen = 0


def keys_since():
    """Key and shortcut lines printed since the last call."""
    global seen
    lines = read(HOTKEY_LOG).splitlines()
    new, seen = lines[seen:], len(lines)
    return [line for line in new if not line.startswith("shortcut:")]


def press(*keys):
    """Plugs in the stand-in keyboard, presses `keys`, unplugs it."""
    machine.succeed("stand-in-keyboard 1.5 " + " ".join(keys), timeout=60)
    time.sleep(0.5)
    return keys_since()


with subtest("the shortcut is read from /dev/input, from a keyboard plugged in later"):
    # The stand-in's keys also reach the desktop; on the bare desktop they
    # would start KRunner, which later takes the focus.
    in_background("GDK_BACKEND=wayland capture-text /tmp/stray.txt", "/tmp/stray.log")
    machine.wait_until_succeeds("test -e /tmp/stray.txt.focused", timeout=60)
    in_background(
        f"{DAEMON} --hotkey-test --hotkey RightAlt --hotkey toggle:RightAlt+Space",
        HOTKEY_LOG,
    )
    machine.wait_until_succeeds(f"grep -q 'shortcut: reading' {HOTKEY_LOG}")
    keys_since()
    got = press("+KEY_RIGHTALT", "1", "-KEY_RIGHTALT")
    assert got == ["RightAlt down", "  down", "  pressed", "RightAlt up", "  released"], got
    machine.succeed(f"grep -q 'shortcut: reading stand-in keyboard' {HOTKEY_LOG}")

with subtest("a tap, or another key with it, is no dictation"):
    got = press("KEY_RIGHTALT")
    assert got == ["RightAlt down", "  down", "RightAlt up", "  cancelled"], got
    got = press("+KEY_RIGHTALT", "0.05", "KEY_E", "-KEY_RIGHTALT")
    assert got == [
        "RightAlt down",
        "  down",
        "E down",
        "  cancelled",
        "E up",
        "RightAlt up",
    ], got

with subtest("another key during the dictation cancels it"):
    got = press("+KEY_RIGHTALT", "0.6", "KEY_E", "-KEY_RIGHTALT")
    assert got == [
        "RightAlt down",
        "  down",
        "  pressed",
        "E down",
        "  cancelled",
        "E up",
        "RightAlt up",
    ], got

with subtest("Space while holding goes hands-free; the next press ends it"):
    got = press("+KEY_RIGHTALT", "0.5", "KEY_SPACE", "-KEY_RIGHTALT", "0.5", "KEY_RIGHTALT")
    assert got == [
        "RightAlt down",
        "  down",
        "  pressed",
        "Space down",
        "  handsFree",
        "Space up",
        "RightAlt up",
        "RightAlt down",
        "  released",
        "RightAlt up",
    ], got

with subtest("a binding's backend comes with its events"):
    machine.succeed("pkill -f -- --hotkey-test")
    in_background(f"{DAEMON} --hotkey-test --hotkey local:RightCtrl", HOTKEY_LOG)
    seen = 0
    machine.wait_until_succeeds(f"grep -q 'shortcut: reading' {HOTKEY_LOG}")
    keys_since()
    got = press("+KEY_RIGHTCTRL", "0.5", "-KEY_RIGHTCTRL")
    assert got == [
        "RightCtrl down",
        "  down local",
        "  pressed local",
        "RightCtrl up",
        "  released local",
    ], got

with subtest("a keyboard unplugged while held lets go of its keys"):
    got = press("+KEY_RIGHTCTRL", "0.5")
    assert got == [
        "RightCtrl down",
        "  down local",
        "  pressed local",
        "RightCtrl up",
        "  released local",
    ], got

# ---------------------------------------------------------------- typing
TYPED = "/tmp/typed.txt"

with subtest("typing into a native Wayland window in KDE's German layout"):
    in_background(f"GDK_BACKEND=wayland capture-text {TYPED}", "/tmp/capture.log")
    machine.wait_until_succeeds(f"test -e {TYPED}.focused", timeout=60)
    # y and z swap places in German; the rest is AltGr.
    first = "Hallo Welt, zy ZY äöü ß € @ {[]} | ~ \\ <> µ"
    type_text(first, focused=TYPED)
    wait_for_text(TYPED, first)

with subtest("a transcript with characters the layout lacks is pasted whole"):
    # ^ and ` are dead keys in German; the emoji is on no layout.
    second = " 😀 ^ ` ą"
    type_text(second, focused=TYPED)
    wait_for_text(TYPED, first + second)
    clipboard = as_alice(f"{WL_COPY.replace('wl-copy', 'wl-paste')} -n")
    assert clipboard == second, clipboard

with subtest("a new line is Shift+Enter"):
    third = "\nzweite Zeile"
    type_text(third, focused=TYPED)
    wait_for_text(TYPED, first + second + third)

with subtest("typing into Konsole"):
    in_background("konsole -e sh -c 'cat > /tmp/konsole.txt'", "/tmp/konsole.log")
    machine.wait_until_succeeds(f"pgrep -u {USER} -x cat", timeout=60)
    time.sleep(3)
    type_text("Konsole zy €@")
    # A plain Enter from the keyboard ends the line for cat.
    machine.succeed("stand-in-keyboard 1.5 KEY_ENTER")
    wait_for_text("/tmp/konsole.txt", "Konsole zy €@\n")
    # What Shift+Enter gives in a terminal, for the record.
    type_text("A\nB")
    machine.succeed("stand-in-keyboard 1.5 KEY_ENTER")
    time.sleep(2)
    log("Konsole got after A, Shift+Enter, B, Enter:", repr(read("/tmp/konsole.txt")))

with subtest("the shortcut never reads the typing keyboard"):
    log_text = read(HOTKEY_LOG)
    assert "reading wispr-headless keyboard" not in log_text, log_text
    for line in keys_since():
        assert line in ("Enter down", "Enter up"), line

machine.screenshot("screen")
