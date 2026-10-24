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
#   barColor fill of the flow bar wispr-headless draws ("#rrggbb"; Wispr's
#            black by default)
#   localAsr transcription on the Mac for the dictations wispr-headless
#            starts itself (see programs.wisprFlow.headless.localAsr in
#            ../wispr-flow.nix for the meaning of each): { llamaServer ?
#            llama-server from this flake's llama.cpp (../llama-cpp),
#            cross-built with stdenv and sdk, or the path of another
#            aarch64-darwin one (b11852 or later);
#            backend ? "local"; model, mmproj, keywords ? null (paths on the
#            Mac; null for the daemon's defaults in ~/Library/Application
#            Support/wispr-headless); idleSeconds ? 0; serverArgs ? [ ];
#            targetSeconds ? 5; maxSeconds ? 10;
#            barColor ? "#15803d", the bar for dictations to the local model }
#   stdenv, sdk  the cross toolchain, as for lib.macLlamaCpp: an stdenv
#            whose cc is an aarch64-apple-darwin clang, and the macOS SDK it
#            uses; only for llama.cpp, so only with localAsr and no
#            llamaServer
#   hotkeys  dictation shortcuts wispr-headless watches itself, through an
#            event tap with Wispr's Accessibility permission (see the NixOS
#            module's headless.hotkeys). Default: hold Right Option to
#            dictate with Wispr and, with localAsr, Right Command with the
#            local model; the left keys stay as they are
#   suppressWisprShortcut  keep Wispr's own dictation shortcuts (Fn by
#            default) from starting a dictation, without changing Wispr's
#            settings (see the NixOS module's headless.suppressWisprShortcut);
#            on unless hotkeys is [ ]
#   typing   how wispr-headless types what the local model heard: "auto" or
#            "paste" (Cmd+V, the clipboard put back after), "keys"
{
  native,
  builders,
  darwin,
  daemon,
  user,
  signingIdentity ? "Wispr Flow local code signing (Nix)",
  barColor ? "#000000",
  localAsr ? null,
  stdenv ? null,
  sdk ? null,
  hotkeys ? null,
  suppressWisprShortcut ? null,
  typing ? "auto",
}:
let
  local = {
    backend = "local";
    model = null;
    mmproj = null;
    keywords = null;
    idleSeconds = 0;
    targetSeconds = 5;
    maxSeconds = 10;
    serverArgs = [ ];
    barColor = "#15803d";
  }
  // (if localAsr == null then { } else localAsr);
  llamaServer =
    local.llamaServer or (
      if stdenv == null || sdk == null then
        throw "macWisprFlow: localAsr needs llamaServer, or stdenv and sdk to cross-build llama.cpp"
      else
        "${native.callPackage ../llama-cpp { inherit stdenv sdk; }}/bin/llama-server"
    );
  hotkeyList =
    if hotkeys != null then
      hotkeys
    else
      [ "wispr:RightOption" ] ++ native.lib.optional (localAsr != null) "local:RightCommand";
  suppress = if suppressWisprShortcut != null then suppressWisprShortcut else hotkeyList != [ ];
  # wispr-headless/daemon/src/asr/mod.rs, Settings.
  asrConfig = native.writeText "wispr-headless-asr.json" (
    builtins.toJSON (
      {
        server = llamaServer;
        inherit (local)
          idleSeconds
          serverArgs
          targetSeconds
          maxSeconds
          ;
      }
      // native.lib.filterAttrs (_: v: v != null) { inherit (local) model mmproj keywords; }
    )
  );
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
    backend = native.lib.optionalString (localAsr != null) local.backend;
    asrConfig = native.lib.optionalString (localAsr != null) "${asrConfig}";
    inherit barColor;
    localBarColor = local.barColor;
    suppressWisprShortcut = native.lib.optionalString suppress "1";
    daemonArgs = builtins.concatStringsSep " " (
      native.lib.concatMap (h: [
        "--hotkey"
        h
      ]) hotkeyList
      ++ [
        "--type-with"
        typing
      ]
    );
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
