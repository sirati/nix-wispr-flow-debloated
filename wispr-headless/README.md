# Wispr Flow without renderers

Wispr Flow's Electron app keeps five hidden windows with a Chromium renderer
each. Its settings window (the Hub) also records dictation and played its
onboarding videos all day. With `programs.wisprFlow.headless.enable`, no
renderer runs while Wispr is in the background:

- `shim.js` runs in Electron's main process before Wispr's code (the Nix
  build points `package.json`'s `main` at it). Wispr gets real
  `BrowserWindow` objects, but their pages load only when needed. Showing a
  window loads it. So does a message the shim does not handle, such as a
  notification for the status window or meeting capture for the Hub. When a
  loaded window goes back to showing nothing, its renderer is killed, and
  Electron starts a fresh one on the next load. Virtual windows acknowledge
  zubridge store updates the way a page would.
- `daemon/` (Rust, `wispr-headless`) is spawned by the shim. It records the
  microphone with `pw-cat` and sends Wispr the same `dictation:*` messages
  the Hub would: 640-sample 16 kHz WAV chunks, then `recordingDone`. It also
  draws the speech indicator as an XWayland window and plays Wispr's sounds.

If the daemon cannot run or exits, every window loads as usual. Nothing
depends on Wispr's minified code: only IPC channel names and the preload
path of each window.

The shim logs names and key paths only, never values, to
`~/.cache/wispr-flow/shim-trace.jsonl`.

## Tests

    cargo test                                  # in daemon/: unit tests + binary with a stand-in mic
    tests/electron/run.sh <wispr-headless>      # shim + daemon in Electron 42, Wispr's window layout

The Electron test needs `electron_42`, `xvfb-run` and `nodejs` on PATH.

## What the trace showed (logged in, one dictation)

`dictation:audioStart {useOpus, transcriptionEntityUuid}` goes to the Hub. The
Hub answers with `recordingStarted` and `audioChunk` messages (544 over
7.4 s), and with `recordingDone` 9 ms after `audioStop`. The status window
gets `status:dictationStatus` (idle, recording, reconnecting, flushing,
compressing), `status:audioLevel`, `status:micActive` and
`notification:clear`. Settings open as `app.second-instance`, then `hub.show`.
