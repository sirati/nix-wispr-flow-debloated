# Wispr Flow, debloated

Nix packaging for [Wispr Flow](https://wisprflow.ai) dictation on NixOS and
macOS, without its Electron app running Chromium in the background. Wispr
Flow is proprietary; this repository has no affiliation with Wispr, and on
Linux it builds on the unofficial
[wispr-flow-linux](https://github.com/wispr-flow-linux/wispr-flow-linux) port.

## What is here

| Path | What it is |
|---|---|
| `wispr-flow.nix` | NixOS module `programs.wisprFlow`: the Linux port, fixed to apply all of its patches, run as a home-manager user service, optionally headless. |
| `wispr-headless/` | The headless mode. `shim.js` runs in Wispr's Electron main process and keeps its windows without a renderer until their UI is needed. `daemon/` (Rust) records the microphone and draws Wispr's flow bar (X11 on Linux, AppKit on macOS). See [its README](wispr-headless/README.md). |
| `wispr-ime.nix`, `wispr-ime/` | NixOS module `programs.wisprIme`: dictated text is typed through fcitx5 (an IBus bridge in safe Rust) instead of the clipboard, with Wispr's helper patched to use it. |
| `dictation-buttons.nix` | NixOS module `services.dictationButtons`: mouse and headset buttons as a dictation key. |
| `wispr-patches/` | Extra patches for Wispr's bundle (the Hub's video painting while hidden). |
| `darwin/` | The Mac app: Wispr's pinned release with the headless shim, installed and signed by an activation script. |
| `tests/ime-vm/` | A GNOME-on-Wayland VM test for wispr-ime. |

## Using it from a NixOS flake

```nix
inputs.wispr-flow-debloated = {
  url = "github:sirati/nix-wispr-flow-debloated";
  inputs.nixpkgs.follows = "nixpkgs";
};
```

Import `wispr-flow-debloated.nixosModules.wispr-flow` (and `wispr-ime`,
`dictation-buttons`, or `default` for all three), then for example:

```nix
programs.wisprFlow = {
  enable = true;
  headless.enable = true;
  users = [ "alice" ];
};
```

The options are documented in each module.

## The Mac app

`lib.macWisprFlow` builds the Mac bundle on Linux. It needs an Apple SDK
cross toolchain from the caller:

```nix
wispr-flow-debloated.lib.macWisprFlow {
  native = pkgs;            # x86_64-linux nixpkgs
  builders = crossBuilders; # trivial builders targeting aarch64-darwin
  darwin = import nixpkgs { system = "aarch64-darwin"; };
  daemon = crossRustPlatformPackage ./wispr-headless/daemon;
}
```

Copy the result to the Mac and run its `bin/activate` with sudo. Activation
copies the app to `/Applications/Wispr Flow.app`, signs it with the
certificate `Wispr Flow local code signing (Nix)` from the System keychain
(created on first activation; its key never leaves the Mac), and restarts
Wispr. The first time, Wispr's own build is moved to
`~/.local/setup/app-backups/`. The app never updates itself; bump `version`
and `hash` in `darwin/wispr-flow.nix` instead. The shim and the Rust daemon
live in the activated profile, so changing them does not copy the 600 MB app
again.

The first start after switching from Wispr's own build asks again for
microphone and accessibility access, and once for Wispr's keychain item
(choose Always Allow). If accessibility looks enabled but dictation does
nothing, reset the stale entry with
`sudo tccutil reset Accessibility com.electron.wispr-flow` and enable it
again. The certificate stays, so later versions keep these permissions.

## Tests

    nix flake check                                  # builds the Rust crates with their tests
    cd wispr-headless/daemon && cargo test           # unit tests + the binary with a stand-in mic
    wispr-headless/tests/electron/run.sh <binary>    # shim + daemon in Electron 42
    tests/ime-vm/run-wispr-ime.sh                    # wispr-ime in a GNOME VM (needs /dev/kvm)
