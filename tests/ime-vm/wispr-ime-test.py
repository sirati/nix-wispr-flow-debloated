# Test script for tests/ime-vm/wispr-ime.nix, appended to the harness's baseline.
# Provided before this: HELPER (path of the patched Wispr helper) and everything in prelude.py.

import json

BUS = "org.wisprime.WisprIme"
SPOKEN = "hallo wörld ß 日本語 — fertig"


def ime_bus_has(name):
    return run_in_session(f"busctl --user list --no-pager | grep -c ' {name} '", check=False).strip() != "0"


def commit_call(text):
    """What a dictation client sends. Returns busctl's reply text, e.g. 'b true'."""
    return run_in_session(
        f"busctl --user call {BUS} /org/wisprime/WisprIme org.wisprime.WisprIme1 Commit s {shlex.quote(text)}",
        check=False,
    ).strip()


def paste_frame(text):
    """The frame Wispr's app writes to the helper's stdin for PasteText (see helper proto.rs):
    pretty JSON, '+' -> '+1', '|' -> '+2', terminated by '|'."""
    body = json.dumps(
        {"HelperAPIRequest": {"PasteText": {"payload": {"text": text}}, "uuid": "stand-in"}},
        indent=2,
        ensure_ascii=False,
    )
    return body.replace("+", "+1").replace("|", "+2") + "|"


def wispr_ime_log(lines=60):
    return journal(units=("wispr-ime.service",), lines=lines)


with subtest("wispr-ime is up: it owns the IBus name and published the address file"):
    try:
        machine.wait_until_succeeds(
            "runuser -u alice -- env " + SESSION_ENV + f" busctl --user list | grep -q {BUS}", timeout=90
        )
    except Exception:
        log("wispr-ime never appeared. Status:\n" + run_in_session(
            "systemctl --user status wispr-ime.service fcitx5.service --no-pager -l", check=False))
        log("journal:\n" + journal(units=("wispr-ime.service", "fcitx5.service"), lines=120))
        log("bus names:\n" + bus_names())
        raise
    status = run_in_session("busctl --user status org.freedesktop.IBus")
    log("IBus name owner:\n" + status)
    assert "wispr-ime" in status, "org.freedesktop.IBus is not owned by wispr-ime"
    files = ibus_address_files()
    log(files)
    assert "IBUS_ADDRESS=unix:path=/run/user/1000/bus,fcitx_random_string=" in files, files
    # mutter creates its input context once it connects
    machine.wait_until_succeeds(
        "runuser -u alice -- env " + SESSION_ENV + " journalctl --user --no-pager -u wispr-ime.service"
        " | grep -q 'input context .* for gnome-shell'",
        timeout=120,
    )
    log("wispr-ime journal:\n" + wispr_ime_log())

with subtest("dead keys compose through mutter -> wispr-ime -> fcitx5 (GTK4 and GTK3)"):
    res = probe_dead_key("wispr")
    for kind, text in res.items():
        assert text.startswith("á"), f"{kind}: expected a-acute, got {text!r}"

with subtest("Super+Space is handled by fcitx5 and not typed"):
    del IM_SWITCH_LOG[:]
    text = type_in_terminal(
        "kgx", [switch_im_with_super_space, switch_im_with_super_space], name="wispr-switch"
    )
    log("IM switches:", IM_SWITCH_LOG)
    assert text.strip() == "", f"Super+Space leaked into the field: {text!r}"
    assert all(b != a for b, a in IM_SWITCH_LOG), f"input method did not change: {IM_SWITCH_LOG}"

with subtest("typing fast keeps its order"):
    out, unit = start_terminal("kgx", "burst")
    burst = "pack my box with five dozen liquor jugs 0123456789 and again " * 2
    machine.send_chars(burst, delay=0.02)
    text = finish_terminal(out, unit)
    assert text.rstrip("\n") == burst, f"typed text was altered: {text!r}"

with subtest("voice stand-in: Commit types into the focused field and leaves the clipboard alone"):
    run_in_session("printf KEEP-ME | (setsid wl-copy >/dev/null 2>&1 &)")
    machine.sleep(1)
    out, unit = start_terminal("kgx", "voice")
    assert "true" in commit_call(""), "probe: a field has focus"
    assert "true" in commit_call(SPOKEN)
    machine.sleep(1)
    text = finish_terminal(out, unit)
    log("voice stand-in captured:", repr(text))
    assert text.startswith(SPOKEN), repr(text)
    clip = run_in_session("wl-paste -n", check=False)
    log("clipboard afterwards:", repr(clip))
    assert clip == "KEEP-ME", f"the dictation touched the clipboard: {clip!r}"
    log("wispr-ime journal:\n" + wispr_ime_log())

with subtest("nothing to commit into when no field has focus"):
    machine.sleep(2)
    answer = commit_call("")
    assert "false" in answer, f"expected false with no focused field, got {answer!r}"

with subtest("the real helper's PasteText goes through wispr-ime (stand-in for Wispr's app)"):
    out, unit = start_terminal("kgx", "helper")
    frame = base64.b64encode(paste_frame(SPOKEN).encode()).decode()
    machine.succeed(f"echo {frame} | base64 -d > /tmp/frame && chmod 644 /tmp/frame")
    # stdin stays open a few seconds so the helper answers before it sees EOF; fd 3 is its reply channel
    # run_in_session appends 2>&1, so the helper's log lines come back as the output
    helper_err = run_in_session(
        f"(cat /tmp/frame; sleep 6) | XDG_SESSION_TYPE=wayland {HELPER} 3>/tmp/fd3.out",
        timeout=90,
        check=False,
    )
    log("helper log:\n" + helper_err)
    log("helper fd3:\n" + machine.succeed("cat /tmp/fd3.out"))
    text = finish_terminal(out, unit)
    assert "committed through wispr-ime" in helper_err, "helper did not use wispr-ime"
    assert text.startswith(SPOKEN), repr(text)
    assert "ACK" in machine.succeed("cat /tmp/fd3.out"), "helper did not acknowledge PasteText"

with subtest("an open terminal keeps working after fcitx5 (and so wispr-ime) restarts, as on a switch"):
    out, unit = start_terminal("kgx", "restart")
    run_in_session("systemctl --user restart fcitx5.service")
    machine.wait_until_succeeds(
        "runuser -u alice -- env " + SESSION_ENV + f" busctl --user list | grep -q {BUS}", timeout=60
    )
    machine.sleep(10)  # mozc loading, mutter reconnecting
    log("wispr-ime journal after restart:\n" + wispr_ime_log(80))
    send_keys("apostrophe", "a")
    text = finish_terminal(out, unit)
    log("after restart:", repr(text))
    assert text.startswith("á"), f"typing did not recover after the restart: {text!r}"

with subtest("a clean stop leaves GNOME with no address instead of a dead one"):
    run_in_session("systemctl --user stop wispr-ime.service")
    machine.sleep(2)
    files = ibus_address_files()
    log(files)
    assert "IBUS_DAEMON_PID=\n" in files or "IBUS_ADDRESS=\n" in files, files
