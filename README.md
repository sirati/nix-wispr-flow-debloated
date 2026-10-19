# Wispr Flow, debloated

Nix packaging for [Wispr Flow](https://wisprflow.ai) dictation on Linux and
macOS. Wispr's Electron app normally keeps five Chromium renderers running in
the background, one of them playing videos in a hidden window. With this
package no renderer runs until Wispr shows a window. A small Rust daemon
records the microphone and draws the flow bar instead.

Wispr Flow is proprietary, and this project has no affiliation with Wispr.
On Linux it builds on the unofficial
[wispr-flow-linux](https://github.com/wispr-flow-linux/wispr-flow-linux)
port. On macOS it uses Wispr's own release.

## How it works

`wispr-headless/shim.js` runs in Electron's main process before Wispr's code.
Wispr still gets real windows, but a window loads its page only when Wispr
shows it or sends it a message the shim does not handle, such as a
notification. Once the window shows nothing again, the shim stops its
renderer. If the daemon cannot run, every window loads as usual.

The daemon in `wispr-headless/daemon` does the work of the hidden windows:

- It records from the microphone selected in Wispr, with CoreAudio on macOS
  and `pw-cat` on Linux, and sends Wispr the same audio messages its own
  recorder sends.
- It starts recording when the dictation shortcut goes down. If another key
  joins before Wispr asks to record, it drops that recording.
- It draws the flow bar following Wispr's own states: the resting bar, the
  push-to-talk and hands-free pills with their buttons, the processing
  spinner, and the "Transcript cancelled" notification with Undo. The pill is
  red, so a black pill means Wispr's own page is drawing.

Nix pins the Wispr version, and the app never updates itself.

## Use

Add the flake input:

```nix
inputs.wispr-flow-debloated = {
  url = "github:sirati/nix-wispr-flow-debloated";
  inputs.nixpkgs.follows = "nixpkgs";
};
```

On NixOS, import `wispr-flow-debloated.nixosModules.default` and enable it:

```nix
programs.wisprFlow = {
  enable = true;
  headless.enable = true;
  users = [ "alice" ];
};
```

`programs.wisprIme` fixes dictation on GNOME. Wispr's helper types text by
copying it to the clipboard and pressing Ctrl+V. GNOME has no protocol that
lets a program set the clipboard without a window, so the copy opens a window
that takes focus and the paste goes to the wrong place. wispr-ime types the
text through the input method instead, with fcitx5 behind a small IBus bridge
written in safe Rust. Other desktops work without it.

`services.dictationButtons` turns mouse or headset buttons into a dictation
key.

On macOS, `lib.macWisprFlow` cross-builds the app on Linux. The caller
provides an Apple SDK toolchain:

```nix
wispr-flow-debloated.lib.macWisprFlow {
  native = pkgs;            # x86_64-linux nixpkgs
  builders = crossBuilders; # trivial builders targeting aarch64-darwin
  darwin = import nixpkgs { system = "aarch64-darwin"; };
  daemon = crossRustPlatformPackage ./wispr-headless/daemon;
  user = "alice";           # the macOS user who runs Wispr
}
```

Copy the result to the Mac and run its `bin/activate` with sudo. The shim
changes the app, which breaks Wispr's signature, so activation installs the
app in `/Applications` and signs it again. It uses a certificate that it
creates once in the System keychain (`signingIdentity`, default `Wispr Flow
local code signing (Nix)`). The private key never leaves the Mac. macOS ties
Wispr's permissions to that certificate, so you grant microphone and
accessibility access once after switching from Wispr's own build. Wispr also
asks once for its keychain item; choose Always Allow. If accessibility looks
enabled but dictation does nothing, run
`sudo tccutil reset Accessibility com.electron.wispr-flow` and enable it
again. The first activation moves Wispr's own build to
`~/.local/setup/app-backups/`.

## Updating Wispr

On Linux, change the rev of the `wispr-flow-linux` input in `flake.nix`. On
macOS, take `currentRelease` from
`https://dl.wisprflow.com/wispr-flow/darwin/arm64/RELEASES.json` and update
`version` and `hash` in `darwin/wispr-flow.nix`.

Then check that the new `app.asar` still has the names the shim uses in
`.webpack/main/index.js`: `dictation:audioStart`, `dictation:audioChunk`,
`recordingStarted`, `recordingDone`, `status:dictationStatus`,
`status:audioLevel`, `status:micActive`, `status:popo`, `status:startClicked`,
`notification:show`, `zubridge:state-update`, `hideFlowBarPermanently`,
`clickableBubbleSnoozeUntil`, and the preload paths
`renderer/<role>/preload.js`.

## Tests

    nix flake check                                  # builds the Rust crates with their tests
    cd wispr-headless/daemon && cargo test           # unit tests and the binary with a stand-in microphone
    wispr-headless/tests/electron/run.sh <binary>    # shim and daemon in Electron 42
    tests/ime-vm/run-wispr-ime.sh                    # wispr-ime in a GNOME VM (needs /dev/kvm)

The Electron test needs `electron_42`, `xvfb-run` and `nodejs` on `PATH`. The
macOS daemon build puts its test binaries in `libexec/tests`. Run them on the
Mac with `WISPR_HEADLESS_BIN=<out>/bin/wispr-headless`.

## Debugging

The shim writes a trace to `~/.cache/wispr-flow/shim-trace.jsonl`. It holds
IPC channel names, key paths and timings, never values or keystrokes. On
macOS, `WISPR_HEADLESS_SNAPSHOT_DIR` makes the daemon save each state of the
flow bar as a TIFF.
