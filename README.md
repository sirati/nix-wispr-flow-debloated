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
`maxSeconds`), so only the last piece is left when it ends.
`localAsr.cleanup.enable` cleans up each transcript with Spokenly's Cleanup 1
(`cleanup-1-q8_0.gguf` from huggingface.co/spokenly/cleanup-1-gguf, in the
same directory or at `cleanup.model`): fillers, repeats and spoken
corrections go; punctuation, capitals and digits come. It runs in a second
llama-server that loads while the last piece is transcribed and stops after
its answer. An answer that looks wrong or takes longer than
`cleanup.timeoutSeconds` leaves the transcript as Granite wrote it. The flow bar is
green while a dictation goes to the local model (`localAsr.barColor`) and
black otherwise (`headless.barColor`).

When the [dictation-api broker](https://github.com/sirati/dictation-api) is
running, the daemon asks it for a destination at the start of each local or
Wispr dictation. The broker alone selects the application. An accepting app
captures its insertion target and receives audio levels and the final text
directly. The Rust bubble shows the app's supplied icon while recording and
processing. No raw audio goes through this API. The provider currently offers
level visualization; spectrum visualization is not advertised.

Without a broker, or when no app accepts, dictation uses its usual insertion
method. Once an app accepts, a delivery failure reports an error without
pasting into another window. Local cancellation keeps the captured session
for Undo; dismissing or expiring the notice cancels that session. Supported
icon formats are PNG, JPEG, WebP, ICO and static SVG, supplied as a data URL
or an absolute filename.

`headless.hotkeys` makes wispr-headless watch dictation shortcuts itself,
from /dev/input on any desktop, for example
`[ "wispr:RightAlt" "local:RightCtrl" ]`: hold Right Alt to dictate with
Wispr, Right Ctrl with the local model, whose text is then typed into the
focused window. A held key becomes a dictation after 200 ms alone; another
key while it is held makes it an ordinary shortcut. A short tap does
nothing, but two presses close together (350 ms between the first release
and the second press, `doublePressMs`) start a hands-free dictation, which
goes on without the key; a press of any of these shortcuts ends it, and
Escape cancels it. Only one dictation records at a time, for Wispr or the
local model, and the one flow bar shows the newest: a press while one
records ends it (and starts nothing), a press while Wispr's own dictation
records starts nothing (it stops a hands-free one of Wispr's), and a press
while one is only being transcribed starts the next one. A local dictation
also ends after `maxDictationSeconds` (600), or with an error if its model
server dies. wispr-headless logs what it does, without what was said, to
`~/Library/Logs/wispr-headless.log` on macOS and
`$XDG_STATE_HOME/wispr-headless/log` on Linux (1 MB, then one older file).
On macOS,
`lib.macWisprFlow` holds Right Option for Wispr and, with `localAsr`, Right
Command for the local model (`hotkeys` changes that). A `wispr:` shortcut
drives Wispr through its own push to talk and hands-free, so Wispr shows and
behaves as on its own shortcut: the shim presses F20 (push to talk) and F19
(hands-free), keys nobody presses, for Wispr (`wisprPushToTalkKey`,
`wisprHandsFreeKey`), and keeps the shortcut keys themselves from Wispr.
While wispr-headless runs, the shim binds Wispr's push to talk and hands-free
to those keys in Wispr's settings (`rebindWisprShortcuts`), so Wispr's
default (Fn on macOS, Ctrl+Win on Linux) no longer starts a dictation; its
other shortcuts stay. Nothing needs changing in Wispr's settings, and
changing these two there does not last. `suppressWisprShortcut`, off by
default with `rebindWisprShortcuts`, instead keeps Wispr's settings as they
are and drops the key events from Wispr's helper that would start one of
its dictations, passing every other key, so Escape, paste-last and the rest
still work. To try it by hand,
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

The Mac build also has an optional Core ML path for Granite's audio encoder:

```nix
localAsr = {
  coreml = {
    encoder = "~/Library/Application Support/wispr-headless/granite_speech_audio_w8a8.mlpackage";
    frames = [ 500 ];
  };
};
```

This needs the converted encoder package, currently kept separately from
this repository, and this flake's patched llama-server. At app launch a
background warm-up loads the speech model and makes a short silent
prediction, preparing Core ML's Neural Engine and Metal caches before the
first dictation. Core ML compiles the package only when its disk cache is
missing or older than the package. The compiled model lives in
`~/Library/Caches/llama-mtmd-coreml` across app restarts and reboots.
macOS may purge caches or rebuild hardware-specific plans after an OS
update; startup warm-up handles that too. Set `coreml.prewarm = false` to
disable startup preparation.

Preparation shares the speech-server slot with dictation: speaking while
it is running waits for that same server rather than loading a second
copy. Once preparation finishes, `idleSeconds` applies as usual. At its
default of zero the model is released, so later dictations still pay the
ordinary load cost. Cache reuse, model loading and encoder timings are
recorded in `~/Library/Logs/wispr-headless.log`. The language model still runs through
Metal. The encoder requests CPU and Neural Engine execution; the actual
device placement is decided by Core ML.

The default loads only the 500-frame function, which handles up to 10 seconds
of audio. Shorter clips are padded and masked. Loading `[ 250 375 500 ]`
allows smaller inputs, but each function may hold another copy of the
weights. In Core ML-only mode, the GGUF file remains as a metadata container
because llama.cpp needs Granite's format and dimensions, but its encoder and
projector weight tensors are not allocated or loaded. Core ML errors or clips
longer than the configured frame buckets fail instead of silently loading the
redundant GGUF encoder. Core ML is disabled unless `coreml` is set.

A standalone test on an A18 Pro MacBook Neo with macOS 26.6 on October 25,
2026 encoded one 9.02-second LibriSpeech clip in 137 ms with the W8A8
candidate, versus 447 ms with the Q4_K Metal encoder. These are medians of
five predictions after the first. Core ML's first result took about 15
seconds on the first process and 1.16 seconds in a later process using the
cache. Its compute plan assigned 1,375 runtime operations to the Neural
Engine. The [measurements and package hashes](llama-cpp/coreml-benchmark.json)
also record process RSS and the memory guard's readings. Process RSS does
not include Neural Engine memory in system services. These standalone
measurements do not establish full dictation latency or total memory use,
which also include the language model, fallback encoder and cleanup model.

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
