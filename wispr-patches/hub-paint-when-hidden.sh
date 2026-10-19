#!/usr/bin/env bash
# hub-paint-when-hidden.sh <path-to-.webpack/main/index.js>
#
# The Hub window is created with `show: false` and, on startup with --hidden,
# never shown. Electron nevertheless reports its page as visible
# (`document.visibilityState === "visible"`) because `paintWhenInitiallyHidden`
# defaults to true, so the Hub keeps rendering: its onboarding pages play a
# looping, muted, autoplay 1080x1080 video (waveform.mp4 and others) that is
# decoded in software. Measured on a laptop: one renderer at 25-38% of a
# core, 1200 wakeups/s, all day, with nothing on screen.
#
# With `paintWhenInitiallyHidden: false` the page starts out hidden, Chromium
# pauses the video and stops animation frames, and the renderer drops to 0%
# (checked with the same Electron 42 and a 1080x1080 looping video: 95% of a
# core before, 0% after). When the Hub is shown, the page becomes visible and
# plays again.
#
# The anchor is the Hub's option object, picked out by its title; the script
# fails the build if it does not match exactly once, so a Wispr update that
# moves it cannot silently drop the fix.

set -u
bundle="${1:-}"
marker='WISPR_HUB_PAINT_WHEN_HIDDEN'

die() { printf 'hub-paint-when-hidden: %s\n' "$*" >&2; exit 1; }

[[ -f $bundle ]] || die "bundle not found: ${bundle:-<none>}"
if grep -qF "$marker" "$bundle"; then
	printf 'hub-paint-when-hidden: already applied\n'
	exit 0
fi

python3 - "$bundle" "$marker" <<'PY' || exit 1
import re
import sys

path, marker = sys.argv[1], sys.argv[2]
src = open(path, encoding="utf-8").read()

# The Hub's BrowserWindow options: ...title:"Flow Hub",...,show:!1,...
pattern = re.compile(r'(title:"Flow Hub",[^{}]{0,300}?show:!1,)')
matches = pattern.findall(src)
if len(matches) != 1:
    sys.exit(f"hub-paint-when-hidden: expected 1 'Flow Hub' option object with show:!1, found {len(matches)}")

patched = pattern.sub(lambda m: m.group(1) + "/*" + marker + "*/paintWhenInitiallyHidden:!1,", src, count=1)
if src.count("paintWhenInitiallyHidden") != 0:
    sys.exit("hub-paint-when-hidden: bundle already mentions paintWhenInitiallyHidden; check by hand")
open(path, "w", encoding="utf-8").write(patched)
print("hub-paint-when-hidden: Hub window now starts hidden")
PY

# The result must still parse, and carry exactly one marker.
node --check "$bundle" || die "patched bundle no longer parses"
[[ $(grep -o -F "$marker" "$bundle" | wc -l) -eq 1 ]] || die "marker count is not 1"
