#!/usr/bin/env bash
# End-to-end test of shim.js with a real Electron and the real wispr-headless
# daemon, using a stand-in app with Wispr's window layout.
#
#   tests/electron/run.sh <path-to-wispr-headless-binary>
#
# Needs electron_42, xvfb-run and nodejs from nixpkgs; builds a scratch copy of
# the app under $TMPDIR with the shim installed the way the Nix build does.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
daemon="${1:?path to wispr-headless}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

cp -r "$here/app" "$work/app"
mkdir -p "$work/app/wispr-shim"
sed -e "s#@daemon@#$daemon#" -e "s#@pwcat@#/bin/true#" -e "s#@micTarget@##" \
	-e "s#@captureCmd@##" -e "s#@deviceName@##" -e "s#@playerCmd@##" -e "s#@fontDir@#${WISPR_TEST_FONT_DIR:-}#" \
	"$here/../../shim.js" > "$work/app/wispr-shim/index.js"
printf '{"name":"wispr-like","main":"wispr-shim/index.js","wisprOriginalMain":"main.js"}\n' > "$work/app/package.json"

# Stand-in microphone: 3 s of a tone, raw s16le mono 16 kHz.
node -e '
	const b = Buffer.alloc(16000 * 3 * 2);
	for (let i = 0; i < 16000 * 3; i++) b.writeInt16LE(Math.round(Math.sin(i / 8) * 6000), i * 2);
	require("fs").writeFileSync(process.argv[1], b);' "$work/tone.raw"

export WISPR_HEADLESS_TEST_CAPTURE="cat $work/tone.raw"
export WISPR_HEADLESS_TEST_IDLE_MS=1500
export XDG_CACHE_HOME="$work/cache"
unset DISPLAY WAYLAND_DISPLAY
xvfb-run -a --server-args="-screen 0 1280x800x24" electron "$work/app"
