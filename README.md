# Wispr Flow, debloated

Nix packaging for [Wispr Flow](https://wisprflow.ai) dictation on Linux and
macOS. Wispr's Electron app normally keeps five Chromium renderers running in
the background. With this package no renderer runs until Wispr shows a
window. A small Rust daemon records the microphone and draws the flow bar
instead.

Wispr Flow is proprietary, and this project has no affiliation with Wispr.
On Linux it builds on the unofficial
[wispr-flow-linux](https://github.com/wispr-flow-linux/wispr-flow-linux)
port. On macOS it uses Wispr's own release.

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
text through the input method instead. Other desktops work without it.

`headless.hotkeys` are dictation shortcuts that wispr-headless reads itself:
keys, mouse buttons (`wispr:BTN_TASK`) and buttons that only show in raw HID
reports, such as a speakerphone's call button
(`wispr:toggle:hid:0b0e:0412/3:1:0`).

`headless.localAsr` transcribes on the machine instead of in Wispr's
service, with IBM's Granite Speech 5.0 (2B) through llama.cpp's
llama-server (`localAsr.llamaCpp`, by default this flake's build). It applies to the dictations wispr-headless starts itself
(its `dictate` command and its shortcuts); Wispr's own shortcut still goes
to Wispr. The model
loads when such a dictation starts and is freed once its text is out. The
model files are not packaged: put `granite-speech-5.0-2b-Q4_K_M.gguf` and
`mmproj-model-q4_k-convdw-f32.gguf` (the encoder, quantized with
`llama-quantize --tensor-type conv_dw=f32 mmproj-model-f16.gguf <out> Q4_K`)
in `~/.local/share/wispr-headless`, or set `model` and `mmproj`. `keywords`
names a file of terms to spell as listed. The model works through the
dictation in pieces of about 5 s while it is spoken (`targetSeconds`,
`maxSeconds`), so only the last piece is left when it ends. The flow bar is
green while a dictation goes to the local model (`localAsr.barColor`) and
black otherwise (`headless.barColor`).

`headless.hotkeys` makes wispr-headless watch dictation shortcuts itself,
from /dev/input on any desktop, for example
`[ "wispr:RightAlt" "local:RightCtrl" ]`: hold Right Alt to dictate with
Wispr, Right Ctrl with the local model, whose text is then typed into the
focused window. A held key becomes a dictation after 200 ms alone; another
key while it is held makes it an ordinary shortcut. On macOS,
`lib.macWisprFlow` holds Right Option for Wispr and, with `localAsr`, Right
Command for the local model (`hotkeys` changes that). With shortcuts of its
own, Wispr's dictation shortcuts (Fn by default on macOS) are switched off
without touching Wispr's settings (`suppressWisprShortcut`): the shim drops
the key events from Wispr's helper that would start one, and passes every
other key, so Escape, paste-last and the rest still work. To try it by hand,
`wispr-headless --hotkey-test --hotkey RightAlt` prints the keys pressed and
what the shortcut does, and `echo text | wispr-headless --type-test` types
into the focused window three seconds later.

On macOS, `lib.macWisprFlow` cross-builds the app on Linux. The caller
provides an Apple SDK toolchain:

```nix
wispr-flow-debloated.lib.macWisprFlow {
  native = pkgs;            # x86_64-linux nixpkgs
  builders = crossBuilders; # trivial builders targeting aarch64-darwin
  darwin = import nixpkgs { system = "aarch64-darwin"; };
  daemon = crossRustPlatformPackage ./wispr-headless/daemon;
  user = "alice";           # the macOS user who runs Wispr
  # Optional: local transcription, models in
  # ~/Library/Application Support/wispr-headless, with llama.cpp built by
  # the same toolchain as lib.macLlamaCpp below.
  localAsr = { };
  stdenv = crossStdenv;
  sdk = appleSdk;
}
```

Copy the result to the Mac and run its `bin/activate` with sudo. The package
changes the app, which breaks Wispr's signature, so activation signs it again
with a certificate that stays in the Mac's System keychain. Grant microphone
and accessibility access once after switching from Wispr's own build; the
certificate keeps them for later versions.

`packages.x86_64-linux.llama-cpp` is llama.cpp pinned to a release that runs
IBM Granite Speech 5.0, with patches in `llama-cpp/patches/` so that an audio
encoder (mmproj) quantized with plain `llama-quantize` works. On macOS,
`lib.macLlamaCpp` cross-builds it with Metal using the same toolchain:

```nix
wispr-flow-debloated.lib.macLlamaCpp {
  native = pkgs;          # x86_64-linux nixpkgs
  stdenv = crossStdenv;   # its cc an aarch64-apple-darwin clang
  sdk = appleSdk;         # the macOS SDK that clang uses
}
```

## Tests

    nix flake check
    cd wispr-headless/daemon && cargo test
    # Local transcription with the real model, in real time:
    WISPR_ASR_SERVER=.../llama-server WISPR_ASR_MODEL=... WISPR_ASR_MMPROJ=... \
      WISPR_ASR_WAVS=a.wav:b.wav cargo test real_model -- --ignored --nocapture
    wispr-headless/tests/electron/run.sh <wispr-headless binary>
    tests/ime-vm/run-wispr-ime.sh
    tests/kde-vm/run.sh

The KDE test runs Plasma on Wayland in a VM with a stand-in keyboard: the
shortcut from /dev/input and typing through /dev/uinput.
