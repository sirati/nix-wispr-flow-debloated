# Wispr Flow dictation, via the unofficial Linux port
# (github:wispr-flow-linux/wispr-flow-linux, not affiliated with Wispr).
#
# Wispr ships no Linux app. The port downloads Wispr's Windows installer,
# patches the Electron bundle for Linux and adds a Rust helper that types the
# text into the focused window. Use is limited to the hosts that import this.
#
# Why this does not just use the port's own flake package: its nix recipe
# reimplements scripts/build-linux.sh but applies only 2 of the 12 patches
# and leaves the Windows sqlite .node files in place, so its own
# verify-patches.sh rejects the build. The overrideAttrs below adds the
# missing steps in build-linux.sh order; replaceOnce makes an upstream
# restructure fail evaluation instead of silently skipping patches again.
#
# Bumping: change the rev of the `wispr-flow-linux` input in flake.nix and
# update the lock. The installer URL and hash come from that rev's
# installer-pin.sh. If native-modules-version.txt moves, the two addon hashes
# below fail to match; fetch the new SHA256SUMS, check the lock's patch_sha256
# against scripts/patches/v8-14.8-better-sqlite3-multiple-ciphers.patch, and
# update them.
{
  config,
  lib,
  pkgs,
  wispr-flow-linux,
  ...
}:

let
  cfg = config.programs.wisprFlow;

  pinFile = builtins.readFile "${wispr-flow-linux}/scripts/setup/installer-pin.sh";
  pinValue =
    name:
    let
      line = lib.findFirst (l: lib.hasPrefix "${name}=" l) null (lib.splitString "\n" pinFile);
      m = if line == null then null else builtins.match "[A-Z_0-9]+='([^']*)'.*" line;
    in
    if m == null then throw "wispr-flow: ${name} not found in installer-pin.sh" else builtins.head m;

  version = pinValue "WISPR_VERSION";

  # The proprietary installer, from Wispr's own download host, verified
  # against the hash pinned in the port's repo.
  installer = pkgs.fetchurl {
    name = "Wispr-Flow-Setup-v${version}.exe";
    url = pinValue "WISPR_INSTALLER_URL";
    sha256 = pinValue "WISPR_INSTALLER_SHA256";
  };

  # Prebuilt Linux sqlite addons for Electron 42 (ABI 146), published by the
  # port at the tag in native-modules-version.txt. Checked against the
  # release's SHA256SUMS and lock (patch_sha256 matches the repo's patch).
  nativeTag = lib.removeSuffix "\n" (
    builtins.readFile "${wispr-flow-linux}/native-modules-version.txt"
  );
  nativeUrl = "https://github.com/wispr-flow-linux/native-modules/releases/download/${nativeTag}";
  betterSqlite3 = pkgs.fetchurl {
    url = "${nativeUrl}/better_sqlite3-x86_64.node";
    hash = "sha256-omxv7HC7bTHDhglJGwORuOrfPZwsxYTVTxYBhX4JDj8=";
  };
  nodeSqlite3 = pkgs.fetchurl {
    url = "${nativeUrl}/node_sqlite3-x86_64.node";
    hash = "sha256-yb0EGfd++ztdOmkf2gTiZfdArY3BlfC1YAPN6skumjQ=";
  };

  replaceOnce =
    from: to: s:
    let
      parts = lib.splitString from s;
    in
    if builtins.length parts != 2 then
      throw "wispr-flow: upstream buildPhase anchor not found exactly once: ${from}"
    else
      builtins.concatStringsSep to parts;

  patchDir = "${wispr-flow-linux}/scripts/patches";

  # With wispr-ime, the helper's PasteText commits text through the input
  # method (see ./wispr-ime/helper-ime-commit.patch) and only pastes when no
  # field can take it. Same source the port builds, so the patch has to match
  # the pinned version; a new helper release fails here until the patch and
  # hash are updated.
  imeEnabled = config.programs.wisprIme.enable;
  helperIme = pkgs.callPackage ./wispr-ime/helper.nix { inherit wispr-flow-linux; };

  withLinuxPatches =
    buildPhase:
    replaceOnce ''if [[ -d "$resources_src/app.asar.unpacked" ]]; then'' ''
        # Everything upstream's recipe skips, in scripts/build-linux.sh order.
        # Each script fails by name.
        for p in helper-env linux-window-frame linux-hub-focusable \
                 linux-deeplink linux-early-singleton \
                 linux-disable-pill-drag linux-main-shortcut-defaults \
                 linux-xdg-data-dir linux-autostart; do
          bash ${patchDir}/$p.sh "$main_bundle" \
            || { echo "wispr-flow: patch $p failed" >&2; exit 1; }
        done
        webpack_root=asar-contents/.webpack
        bash ${patchDir}/linux-renderer-chrome.sh \
          "$webpack_root/renderer/hub/index.js" \
          || { echo "wispr-flow: patch linux-renderer-chrome failed" >&2; exit 1; }
        renderers=0
        for r in "$webpack_root"/renderer/*/index.js; do
          [[ -f "$r" ]] || continue
          grep -qF 'platform?.isWindows' "$r" || continue
          bash ${patchDir}/linux-renderer-treat-as-windows.sh "$r" \
            || { echo "wispr-flow: treat-as-windows failed on $r" >&2; exit 1; }
          renderers=$((renderers + 1))
        done
        [[ "$renderers" -gt 0 ]] \
          || { echo "wispr-flow: no renderer reads platform.isWindows" >&2; exit 1; }

        # Our own patch, not the port's: the Hub window, started hidden, would
        # otherwise keep playing its onboarding videos (25-95% of a core, all
        # day). See the script for the measurements.
        bash ${./wispr-patches/hub-paint-when-hidden.sh} "$main_bundle" \
          || { echo "wispr-flow: patch hub-paint-when-hidden failed" >&2; exit 1; }

        # Linux sqlite addons in place of the Windows PE ones.
        nm_rel=.webpack/main/native_modules/build/Release
        mkdir -p "asar-contents/$nm_rel"
        install -m755 ${betterSqlite3} "asar-contents/$nm_rel/better_sqlite3.node"
        install -m755 ${nodeSqlite3} "asar-contents/$nm_rel/node_sqlite3.node"

        # The patch scripts leave <bundle>.orig copies; do not pack them.
        find asar-contents -type f -name '*.orig' -delete
${lib.optionalString (cfg.shim != null) ''
        # Our Electron shim runs first: package.json's main points at it, and
        # it loads the app's own entry, kept in wisprOriginalMain.
        install -Dm644 ${cfg.shim} asar-contents/wispr-shim/index.js
        node -e '
          const f = "asar-contents/package.json", p = JSON.parse(require("fs").readFileSync(f, "utf8"));
          if (p.wisprOriginalMain) throw new Error("package.json already shimmed");
          p.wisprOriginalMain = p.main; p.main = "wispr-shim/index.js";
          require("fs").writeFileSync(f, JSON.stringify(p, null, 2));'
      ''}

        if [[ -d "$resources_src/app.asar.unpacked" ]]; then'' buildPhase;

  # Upstream copies its helper to stage/Release and then makes it executable;
  # put ours there instead (install replaces the read-only copy).
  withImeHelper =
    buildPhase:
    replaceOnce "chmod 0755 stage/Release/wispr-flow-linux-helper" ''
      install -m755 ${helperIme}/bin/wispr-flow-linux-helper stage/Release/wispr-flow-linux-helper
      chmod 0755 stage/Release/wispr-flow-linux-helper'' buildPhase;

  base = (pkgs.callPackage "${wispr-flow-linux}/nix/wispr-flow.nix" { }).overrideAttrs (old: {
    # Replaces the recipe's `src`, which would otherwise import a local .exe.
    src = installer;

    buildPhase =
      let
        patched = withLinuxPatches old.buildPhase;
      in
      if imeEnabled then withImeHelper patched else patched;

    # After asar pack: the unpacked tree must hold the Linux addons too.
    postBuild = ''
      nm_rel=.webpack/main/native_modules/build/Release
      mkdir -p "stage/app.asar.unpacked/$nm_rel"
      install -m755 ${betterSqlite3} "stage/app.asar.unpacked/$nm_rel/better_sqlite3.node"
      install -m755 ${nodeSqlite3} "stage/app.asar.unpacked/$nm_rel/node_sqlite3.node"
    ''
    + (old.postBuild or "");
  });

  package = pkgs.callPackage "${wispr-flow-linux}/nix/fhs.nix" { wispr-flow = base; };
in
{
  options.programs.wisprFlow = {
    enable = lib.mkEnableOption "Wispr Flow dictation (unofficial Linux port)";

    shim = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = ''
        JavaScript file that runs in Electron's main process before the app's
        own code: package.json's main points at it, and it must load the
        app's entry from package.json's wisprOriginalMain.
      '';
    };

    package = lib.mkOption {
      type = lib.types.package;
      default = package;
      defaultText = lib.literalMD "the port's FHS package with the full Linux patch set";
      description = "The wispr-flow package to install.";
    };

    users = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = ''
        Users added to the `input` group. Push-to-talk on Wayland reads the
        keyboard from /dev/input/event*, which only that group (or an ACL for
        the active seat) can open.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];

    # 70-wispr-flow-uinput.rules: write access to /dev/uinput, which the
    # helper types through.
    services.udev.packages = [ cfg.package ];
    boot.kernelModules = [ "uinput" ];

    users.users = lib.genAttrs cfg.users (_: {
      extraGroups = [ "input" ];
    });

    # Run the app under XWayland. Its status pill is an always-on-top window,
    # and Mutter only honours that for X11 windows; as a native Wayland client
    # it would sit behind whatever has focus. Text injection and the
    # keyboard monitor still use the Wayland paths. The price is blurry text
    # on fractional-scale displays.
    environment.sessionVariables.WISPR_USE_X11 = "1";

    # Start it hidden at login so the pill is up in every session.
    environment.etc."xdg/autostart/wispr-flow.desktop".text = ''
      [Desktop Entry]
      Type=Application
      Name=Wispr Flow
      Exec=${cfg.package}/bin/wispr-flow --hidden
      X-GNOME-Autostart-enabled=true
    '';
  };
}
