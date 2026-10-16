# Python prelude for tests/ime-vm/default.nix (spliced into the NixOS test script).
# Provided by default.nix before this: IM_VARIANT, PATH_KGX, PATH_GNOME_TERMINAL, PATH_SH, PATH_CAT.
# `machine` and `start_all` come from the NixOS test driver.

import base64
import shlex
import time

USER = "alice"
UID = 1000
SESSION_ENV = (
    f"HOME=/home/{USER} XDG_RUNTIME_DIR=/run/user/{UID} "
    f"DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{UID}/bus WAYLAND_DISPLAY=wayland-0"
)


def log(*a):
    print("[ime-vm]", *a, flush=True)


# ------------------------------------------------------------------ running things as alice
def run_in_session_status(cmd, timeout=60):
    """Run `cmd` (sh -c) as alice with her session-bus env. Returns (exit status, stdout+stderr)."""
    full = f"runuser -u {USER} -- env {SESSION_ENV} {PATH_SH} -c {shlex.quote(cmd + ' 2>&1')}"
    return machine.execute(full, timeout=timeout)


def run_in_session(cmd, timeout=60, check=True):
    """Like machine.succeed, but as alice with XDG_RUNTIME_DIR/DBUS_SESSION_BUS_ADDRESS/WAYLAND_DISPLAY set."""
    status, out = run_in_session_status(cmd, timeout=timeout)
    if check and status != 0:
        raise Exception(f"run_in_session({cmd!r}) failed with status {status}:\n{out}")
    return out


def shell_eval(js):
    """Evaluate JS inside gnome-shell (needs --unsafe-mode, set by default.nix). Returns the JSON text
    result as a str (quotes stripped for plain strings), or raises."""
    out = run_in_session(
        "gdbus call --session -d org.gnome.Shell -o /org/gnome/Shell "
        f"-m org.gnome.Shell.Eval {shlex.quote(js)}"
    ).strip()
    # (true, '"org.gnome.Console"')  /  (true, 'false')  /  (false, '')
    if not out.startswith("(true,"):
        raise Exception(f"Shell.Eval({js!r}) -> {out}")
    val = out[len("(true,") :].strip()
    val = val[:-1] if val.endswith(")") else val
    val = val.strip()
    if val.startswith("'") and val.endswith("'"):
        val = val[1:-1]
    if val.startswith('"') and val.endswith('"'):
        val = val[1:-1]
    return val


# ------------------------------------------------------------------ boot
def boot_to_desktop():
    """GDM autologin -> alice's GNOME Wayland session fully started, overview closed."""
    machine.wait_for_unit("display-manager.service")
    machine.wait_for_file(f"/run/user/{UID}/wayland-0")
    machine.wait_for_unit("default.target", USER)
    machine.wait_until_succeeds(
        "runuser -u alice -- env "
        + SESSION_ENV
        + " gdbus call --session -d org.gnome.Shell -o /org/gnome/Shell -m org.gnome.Shell.Eval "
        "Main.layoutManager._startingUp | grep -q 'true,..false'",
        timeout=300,
    )
    shell_eval("Main.overview.hide()")
    machine.sleep(2)


# ------------------------------------------------------------------ introspection
def session_environment():
    """The systemd user manager's environment (what D-Bus/systemd-activated apps and user services get)."""
    return run_in_session("systemctl --user show-environment")


def proc_env(pattern):
    """Selected IM-related env vars of the (first) process of alice matching pgrep -f `pattern`."""
    return run_in_session(
        f"pid=$(pgrep -u {USER} -f {shlex.quote(pattern)} | head -1); "
        'echo "pid=$pid"; '
        "tr '\\0' '\\n' < /proc/$pid/environ | grep -E '^(GTK_IM_MODULE|QT_IM_MODULE|XMODIFIERS|SKIP_FCITX_USER_PATH|IBUS_[A-Z_]*|WAYLAND_DISPLAY)=' | sort",
        check=False,
    )


def fcitx_current_im():
    """`fcitx5-remote -n` (current input method name), or an error string."""
    return run_in_session("fcitx5-remote -n", check=False).strip()


def ibus_address_files():
    """All ibus address files under alice's config dir, with contents."""
    return run_in_session(
        "ls -la --time-style=full-iso ~/.config/ibus/bus 2>&1; "
        "for f in ~/.config/ibus/bus/*; do [ -e \"$f\" ] && { echo \"== $f\"; cat \"$f\"; }; done; true",
        check=False,
    )


def bus_names():
    return run_in_session(
        "busctl --user list --no-pager | grep -i -E 'ibus|fcitx' ; true", check=False
    )


def ime_state(label=""):
    """Print the facts that matter for the IBus/mutter <-> fcitx5 link."""
    log(f"===== IME STATE {label} =====")
    log("machine-id:", machine.succeed("cat /etc/machine-id").strip())
    log("systemd user env (IM related):\n" + "\n".join(
        l for l in session_environment().splitlines()
        if any(k in l for k in ("IM_MODULE", "XMODIFIERS", "SKIP_FCITX", "IBUS"))
    ))
    log("gnome-shell proc env:\n" + proc_env("gnome-shell --unsafe-mode"))
    log("fcitx5 proc env:\n" + proc_env("fcitx5 -r"))
    log("ibus address files:\n" + ibus_address_files())
    log("bus names (ibus/fcitx):\n" + bus_names())
    log("processes:\n" + run_in_session("ps -u alice -o pid,etime,args | grep -E 'ibus|fcitx5' | grep -v grep; true", check=False))
    log("fcitx5-remote -n:", fcitx_current_im())


def journal(units=("fcitx5.service", "org.gnome.Shell@wayland.service"), grep=None, lines=200):
    """Alice's user-journal lines for the given units (optionally filtered by an egrep pattern)."""
    u = " ".join(f"--user-unit={x}" for x in units)
    cmd = f"journalctl --user --no-pager -o short-monotonic -n {lines} {u}"
    if grep:
        cmd += f" | grep -E {shlex.quote(grep)}"
    return run_in_session(cmd + "; true", check=False)


# ------------------------------------------------------------------ terminal driving
_TERMS = {
    # kind: (wm_class regex-ish substring(s), argv builder)
    "kgx": (("Console",), lambda inner: [PATH_KGX, "-e", inner]),
    "gnome-terminal": (("Terminal", "terminal"), lambda inner: [PATH_GNOME_TERMINAL, "--", PATH_SH, "-c", inner]),
}
_term_n = 0


def focused_wm_class():
    return shell_eval(
        "global.display.focus_window ? global.display.focus_window.get_wm_class() : ''"
    )


def wait_for_focus(substrings, timeout=60):
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        try:
            last = focused_wm_class()
        except Exception as e:  # shell busy
            last = f"<{e}>"
        if any(s in last for s in substrings):
            return last
        time.sleep(0.5)
    machine.screenshot("focus-timeout")
    raise Exception(f"window {substrings} never got focus (last focus: {last!r})")


def start_terminal(kind, name):
    """Start a terminal in alice's session whose stdin is written to /tmp/out-<name>.txt.
    Waits until its window is focused. Returns (output path, systemd unit name)."""
    global _term_n
    _term_n += 1
    wm, build = _TERMS[kind]
    out = f"/tmp/out-{name}.txt"
    machine.succeed(f"rm -f {out}")
    inner = f"{PATH_CAT} > {out}"
    if kind == "kgx":
        inner = f"{PATH_SH} -c {shlex.quote(inner)}"
    argv = build(inner)
    unit = f"ime-test-{name}-{_term_n}"
    # systemd-run --user => same environment as any D-Bus/systemd launched GNOME app
    # (the systemd user manager's environment, i.e. what the GDM session exported).
    run_in_session(
        f"systemd-run --user --collect --quiet --unit={unit} "
        f"--setenv=WAYLAND_DISPLAY=wayland-0 {shlex.join(argv)}"
    )
    machine.wait_for_file(out)  # shell inside the terminal is up and cat has opened the file
    focus = wait_for_focus(wm)
    machine.sleep(2)  # let the toolkit enable its input context after focus-in
    log(f"{kind} focused ({focus}); app env:\n" + proc_env(PATH_KGX if kind == "kgx" else "gnome-terminal-server"))
    return out, unit


def send_keys(*keys, delay=0.4):
    """machine.send_key for each key (QEMU key names: apostrophe, a, meta_l-spc, ctrl-d, ret, ...).
    QEMU sends physical keys; the guest layout (gb) decides the character."""
    for k in keys:
        if callable(k):  # hook, e.g. lambda: log(fcitx_current_im())
            k()
            continue
        machine.send_key(k)
        time.sleep(delay)


def finish_terminal(out, unit):
    """ret + ctrl-d (EOF for `cat`), then read the captured file as UTF-8."""
    send_keys("ret", "ctrl-d", delay=0.6)
    machine.sleep(1)
    run_in_session(f"systemctl --user stop {unit}", check=False)
    raw = base64.b64decode(machine.succeed(f"base64 -w0 {out}"))
    return raw.decode("utf-8", errors="replace")


def type_in_terminal(kind, keys, name=None, pre=None):
    """Whole flow: start terminal `kind` ("kgx" | "gnome-terminal"), optionally call pre(),
    send `keys` (list of QEMU key names), finish, return the captured text (incl. the trailing newline)."""
    name = name or f"{kind}-{_term_n + 1}"
    out, unit = start_terminal(kind, name)
    if pre:
        pre()
    send_keys(*keys)
    text = finish_terminal(out, unit)
    log(f"{kind}: keys={keys} -> captured {text!r}")
    machine.screenshot(f"term-{name}")
    return text


IM_SWITCH_LOG = []


def switch_im_with_super_space():
    """Hook for type_in_terminal's key list: press Super+Space (a terminal must be focused, fcitx5 only
    handles the trigger key for a focused input context) and record fcitx5's current IM before/after."""
    before = fcitx_current_im()
    send_keys("meta_l-spc", delay=1.5)
    after = fcitx_current_im()
    log(f"Super+Space: fcitx5-remote -n  {before!r} -> {after!r}")
    IM_SWITCH_LOG.append((before, after))


# ------------------------------------------------------------------ ready-made probes (they only record, never assert)
def probe_dead_key(tag, kinds=("kgx", "gnome-terminal")):
    """apostrophe + a in each terminal. With keyboard-gb-intl active the dead acute must give U+00E1.
    Returns {kind: captured text}."""
    res = {}
    for kind in kinds:
        text = type_in_terminal(kind, ["apostrophe", "a"], name=f"{tag}-{kind}")
        res[kind] = text
        verdict = "ACUTE (a-acute, IM works)" if text.startswith("á") else "NO ACUTE"
        log(f"RESULT {tag} dead-key {kind}: {text!r} -> {verdict}")
    return res


def probe_super_space(tag, kinds=("kgx", "gnome-terminal")):
    """Super+Space twice in each terminal; records fcitx5's current IM after each press."""
    for kind in kinds:
        del IM_SWITCH_LOG[:]
        type_in_terminal(
            kind, [switch_im_with_super_space, switch_im_with_super_space], name=f"{tag}-sw-{kind}"
        )
        log(f"RESULT {tag} super-space {kind}: (before, after) per press = {IM_SWITCH_LOG}")


def timeline(tag=""):
    """Unit start times (seconds since boot) and address-file mtimes (same scale), for ordering questions."""
    out = run_in_session(
        "for u in gnome-session-pre.target org.gnome.Shell@wayland.service graphical-session.target fcitx5.service ibus-address-file.service; do "
        "  t=$(systemctl --user show -p ActiveEnterTimestampMonotonic --value $u 2>/dev/null); "
        "  [ -n \"$t\" ] && [ \"$t\" != 0 ] && echo \"$u active at $(( t / 1000000 )).$(( (t / 1000) % 1000 ))s\"; "
        "done; "
        "up=$(cut -d' ' -f1 /proc/uptime); now=$(date +%s.%N); "
        "for f in ~/.config/ibus/bus/*; do [ -e \"$f\" ] && echo \"$f mtime at $(awk -v up=$up -v now=$now -v m=$(stat -c %.9Y $f) 'BEGIN{printf \"%.2f\", up-(now-m)}')s\"; done; "
        "echo \"uptime now ${up}s\"; true",
        check=False,
    )
    log(f"TIMELINE {tag}\n" + out)


def probe_fcitx_restart(tag):
    """Restart fcitx5.service, wait for it, dump state, and run the dead-key probe again."""
    run_in_session("systemctl --user restart fcitx5.service")
    machine.wait_until_succeeds(
        "runuser -u alice -- env " + SESSION_ENV + " busctl --user list | grep -q org.freedesktop.IBus",
        timeout=60,
    )
    machine.sleep(10)  # mozc loading, ibus frontend recheck (1.5 s), mutter reconnect
    ime_state(f"{tag} after fcitx5 restart")
    return probe_dead_key(f"{tag}-restart")


def rewrite_ibus_file(tag):
    """(Re)write the IBus address file for wayland-0 with a new `,fcitx_random_string=<tag>` suffix and
    the user systemd manager's pid. libibus only reconnects when the address STRING changed."""
    run_in_session(
        "d=$HOME/.config/ibus/bus; mkdir -p $d; "
        "printf 'IBUS_ADDRESS=unix:path=%s/bus,fcitx_random_string=" + tag + "\\nIBUS_DAEMON_PID=%s\\n' "
        "\"$XDG_RUNTIME_DIR\" \"$(pgrep -u alice -x systemd | head -1)\" "
        "> $d/$(cat /etc/machine-id)-unix-wayland-0"
    )
    log(f"rewrote ibus address file with random string {tag}")
