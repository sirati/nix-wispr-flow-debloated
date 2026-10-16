# GNOME input-method VM tests

A NixOS VM with GNOME on Wayland, fcitx5 configured like the laptop, and
terminals whose input is captured to a file. `default.nix` is the harness,
`prelude.py` its Python helpers; `wispr-ime.nix` plus `wispr-ime-test.py` test
wispr-ime and the patched Wispr helper on top of it.

    tests/ime-vm/run-wispr-ime.sh        # needs /dev/kvm; about 5 minutes
    tests/ime-vm/run-wispr-ime.sh -i     # interactive driver

The voice is a stand-in: the test sends the helper the same `PasteText` frame
Wispr's app sends once it has a transcript, and calls
`org.wisprime.WisprIme1.Commit` directly.

## What the baseline experiments showed

Measured with this harness (nixpkgs GNOME 50.4, fcitx5 5.1.23), typing an
apostrophe then `a` on the gb-intl layout, which must give `á`:

| Setup | Result |
|---|---|
| `GTK_IM_MODULE=fcitx` (the laptop before wispr-ime) | works, but apps bypass GNOME's own input path |
| text-input-v3 path, `SKIP_FCITX_USER_PATH=1` | `'a`: dead |
| same, fcitx5 allowed to write the address file | works |
| same, a static address file written before gnome-shell starts | dead (see below) |
| same, file appears after gnome-shell started | works |
| `IBUS_ADDRESS` or `IBUS_ADDRESS_FILE` instead of the file | dead |

- fcitx5 cannot write `~/.config/ibus/bus/<machine-id>-unix-wayland-0` while
  `SKIP_FCITX_USER_PATH=1` (NixOS `ignoreUserConfig`), so GNOME never finds it.
  wispr-ime writes the file itself.
- libibus reconnects only when the address string changes. If GNOME Shell
  connected while nobody owned `org.freedesktop.IBus`, it finds no engines,
  takes `IBus.Panel` itself and never retries, until the file is rewritten with a
  new address. wispr-ime owns the name first and writes a fresh
  `fcitx_random_string` on every start.
- A restart that leaves the address unchanged leaves GNOME talking to a dead
  input context: typing then produces nothing, not even `'a`.
- The harness found an ordering cycle in the first version of the service:
  wispr-ime after fcitx5, fcitx5 after the graphical session, and the session
  target ordering itself after what it pulls in. Order such a service after
  `graphical-session.target` explicitly.
