# Wispr Flow for the Mac, pinned by Nix and without Chromium renderers until
# its UI is needed (the same headless shim as on Linux).
#
# The app is Wispr's own release zip with its app.asar (and app.asar.unpacked)
# moved, unchanged, to Resources/wispr/, and a Resources/app/ folder holding a
# copy of the app's package.json whose main is a small loader: it requires the
# shim from the activated profile, then Wispr from the moved app.asar. (So a
# new shim or wispr-headless does not change the 600 MB app store path.)
# Electron looks for Resources/app.asar first and Resources/app next (Wispr
# leaves the OnlyLoadAppFromAsar fuse off). The asar keeps its file name
# because Wispr finds the Jabra connector by replacing
# "app.asar" with "app.asar.unpacked" in its path. Only the key of the asar's
# integrity entry in Info.plist changes; the hash is still Wispr's. The shim
# spawns wispr-headless (cross-built, flow bar via AppKit) and records with
# ffmpeg.
#
# These changes break Wispr's signature on the outer bundle, so the
# activation script (wispr-flow-activate.sh) copies the app to /Applications
# and signs it there with a certificate that exists only in that Mac's System
# keychain. The certificate stays the same, so macOS keeps the microphone and
# accessibility permissions across versions. Nested code (Electron, Wispr's
# Swift helper) keeps Wispr's signatures. The app never updates itself; the
# shim answers every update check with "no update".
#
# Bumping: take the version and URL from
# https://dl.wisprflow.com/wispr-flow/darwin/arm64/RELEASES.json (currentRelease),
# update `version` and `hash`, and check the shim's IPC names still exist in
# the new app.asar (listed at the top of wispr-headless/shim.js).
#
# Arguments (see flake.nix, lib.macWisprFlow):
#   native   nixpkgs for the build machine (x86_64-linux)
#   builders trivial builders (runCommand) whose output is for aarch64-darwin
#   darwin   nixpkgs for aarch64-darwin, for ffmpeg from the binary cache
#   daemon   wispr-headless cross-built for aarch64-darwin
#   user     the macOS user who runs Wispr
#   signingIdentity  name of the local code-signing certificate; created on
#            first activation, and kept so macOS keeps Wispr's permissions
{
  native,
  builders,
  darwin,
  daemon,
  user,
  signingIdentity ? "Wispr Flow local code signing (Nix)",
}:
let
  version = "1.6.1174";
  release = native.fetchurl {
    name = "wispr-flow-darwin-arm64-${version}.zip";
    url = "https://dl.wisprflow.com/wispr-flow/darwin/arm64/Wispr%20Flow-darwin-arm64-${version}.zip";
    hash = "sha256-uNNqXjUkOS7DCHoI7/cUdzfqAAys6QUD7IjVfkiDUC0=";
  };

  # From the binary cache; the build machine only downloads it.
  ffmpeg = darwin.ffmpeg-headless.bin;

  shim = native.replaceVars ../wispr-headless/shim.js {
    daemon = "${daemon}/bin/wispr-headless";
    pwcat = "";
    micTarget = "";
    # The default input, 16 kHz mono s16 on stdout, flushed per packet.
    captureCmd = builtins.concatStringsSep " " [
      "${ffmpeg}/bin/ffmpeg -nostdin -hide_banner -loglevel error"
      "-fflags nobuffer -probesize 32 -analyzeduration 0"
      "-f avfoundation -i :default"
      "-ac 1 -ar 16000 -f s16le -flush_packets 1 -"
    ];
    deviceName = "Default (macOS)";
    # The bar uses the system font.
    fontDir = "";
    playerCmd = "/usr/bin/afplay";
  };

  profile = "/nix/var/nix/profiles/mac-wispr-flow";
  loader = native.writeText "wispr-shim-loader.js" ''
    // Loads the headless shim from the activated Nix profile
    // (darwin/wispr-flow.nix), then Wispr's own entry. Without the profile,
    // Wispr runs as usual.
    "use strict";
    const path = require("path");
    globalThis.wisprShimLoader = true;
    try {
      require("${profile}/libexec/wispr-shim.js");
    } catch (e) {
      console.error("wispr-shim: not loaded:", e);
    }
    const root = path.join(__dirname, "..");
    require(path.join(root, require(path.join(root, "package.json")).wisprOriginalMain));
  '';

  app =
    native.runCommand "wispr-flow-macos-app-${version}"
      {
        nativeBuildInputs = [
          native.unzip
          native.asar
          native.nodejs
        ];
      }
      ''
        mkdir -p "$out/Applications"
        cd "$out/Applications"
        unzip -q ${release}
        res="Wispr Flow.app/Contents/Resources"
        [[ -f "$res/app.asar" && ! -e "$res/app" ]]
        version=$(sed -n '/CFBundleShortVersionString/{n;s/.*<string>\(.*\)<\/string>.*/\1/p;}' "Wispr Flow.app/Contents/Info.plist")
        [[ "$version" == ${version} ]] || { echo "zip is $version, expected ${version}" >&2; exit 1; }

        # Electron validates every asar in the bundle against the entry for its
        # path (relative to Contents) in Info.plist.
        mkdir "$res/wispr"
        mv "$res/app.asar" "$res/app.asar.unpacked" "$res/wispr/"
        substituteInPlace "Wispr Flow.app/Contents/Info.plist" \
          --replace-fail "<key>Resources/app.asar</key>" "<key>Resources/wispr/app.asar</key>"

        # The shim itself lives in the activated profile, so changing it (or
        # wispr-headless) does not change this 600 MB store path.
        mkdir -p "$res/app/wispr-shim"
        cp ${loader} "$res/app/wispr-shim/index.js"
        (cd "$res/wispr" && asar extract-file app.asar package.json && mv package.json ../app/package.json)
        node -e '
          const fs = require("fs"), f = process.argv[1];
          const p = JSON.parse(fs.readFileSync(f, "utf8"));
          if (p.wisprOriginalMain) throw new Error("package.json already shimmed");
          p.wisprOriginalMain = "../wispr/app.asar/" + p.main.replace(/^\.\//, "");
          p.main = "wispr-shim/index.js";
          fs.writeFileSync(f, JSON.stringify(p, null, 2) + "\n");
        ' "$res/app/package.json"
      '';
in
builders.runCommand "mac-wispr-flow"
  {
    dontPatchShebangs = true;
    meta.platforms = [ "aarch64-darwin" ];
    passthru = { inherit app daemon shim; };
  }
  ''
    mkdir -p "$out/bin" "$out/libexec"
    ln -s ${app}/Applications "$out/Applications"
    cp ${shim} "$out/libexec/wispr-shim.js"
    cp ${./wispr-flow-activate.sh} "$out/bin/activate"
    substituteInPlace "$out/bin/activate" \
      --replace-fail '@bundle@' "$out" \
      --replace-fail '@version@' '${version}' \
      --replace-fail '@user@' ${native.lib.escapeShellArg user} \
      --replace-fail '@identity@' ${native.lib.escapeShellArg signingIdentity}
    chmod 755 "$out/bin/activate"
  ''
