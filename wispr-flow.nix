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
#
# flake.nix applies this to the pinned wispr-flow-linux source; the result is
# the NixOS module.
{ wispr-flow-linux }:
{
  config,
  lib,
  pkgs,
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

  headlessDaemon = pkgs.callPackage ./wispr-headless/daemon { };
  local = cfg.headless.localAsr;
  # The flow bar's fills (--bar-color).
  colour = lib.types.strMatching "#[0-9a-fA-F]{6}";
  # wispr-headless/daemon/src/asr/mod.rs, Settings; unset paths take the
  # daemon's defaults.
  asrConfig = pkgs.writeText "wispr-headless-asr.json" (
    builtins.toJSON (
      {
        server = lib.getExe' local.llamaCpp "llama-server";
        inherit (local)
          idleSeconds
          serverArgs
          targetSeconds
          maxSeconds
          ;
      }
      // lib.filterAttrs (_: v: v != null) { inherit (local) model mmproj keywords; }
    )
  );
  # Its own dictation shortcuts, and how it types a transcript it finishes
  # itself; wl-copy is for pasting on Wayland.
  daemonArgs = lib.concatStringsSep " " (
    lib.concatMap (h: [
      "--hotkey"
      h
    ]) cfg.headless.hotkeys
    ++ [
      "--type-with"
      cfg.headless.typing
      "--wl-copy"
      "${pkgs.wl-clipboard}/bin/wl-copy"
    ]
  );
  headlessShim = pkgs.replaceVars ./wispr-headless/shim.js {
    daemon = "${headlessDaemon}/bin/wispr-headless";
    pwcat = "${pkgs.pipewire}/bin/pw-cat";
    micTarget = cfg.headless.micTarget;
    # The flow bar's text: Figtree, the font of Wispr's own page.
    fontDir = "${pkgs.figtree}/share/fonts/truetype";
    backend = lib.optionalString local.enable local.backend;
    asrConfig = lib.optionalString local.enable "${asrConfig}";
    barColor = cfg.headless.barColor;
    localBarColor = local.barColor;
    inherit daemonArgs;
    # macOS only (darwin/wispr-flow.nix).
    captureCmd = "";
    deviceName = "";
    playerCmd = "";
  };

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

    headless = {
      enable = lib.mkEnableOption ''
        running Wispr without its windows' renderers until their UI is needed:
        wispr-headless records dictation and draws the speech indicator, and
        the Electron shim (wispr-headless/shim.js) loads Wispr's own pages
        only for settings, notifications and other UI''
      ;
      micTarget = lib.mkOption {
        type = lib.types.str;
        default = "";
        example = "alsa_input.usb-0b0e_Jabra_SPEAK_410_USB-00.mono-fallback";
        description = ''
          PipeWire node to record dictation from; empty for the default
          input. Wispr's own microphone setting does not apply here.
        '';
      };

      barColor = lib.mkOption {
        type = colour;
        default = "#000000";
        example = "#c81414";
        description = ''
          Fill of the flow bar wispr-headless draws, at rest and for Wispr's
          dictations. Wispr's own is black; another colour shows whether
          wispr-headless or Wispr's page draws the bar.
        '';
      };

      localAsr = {
        enable = lib.mkEnableOption ''
          transcribing the dictations wispr-headless starts itself (its
          `dictate` command) with IBM's Granite Speech on this machine,
          through llama.cpp, instead of Wispr's service. The model is loaded
          when such a dictation starts and freed after it. Dictations Wispr
          starts (its own shortcut) still go to Wispr''
        ;
        backend = lib.mkOption {
          type = lib.types.enum [
            "local"
            "wispr"
          ];
          default = "local";
          description = ''
            Where the dictations wispr-headless starts itself go, unless one
            names its own: the local model, or Wispr (through its bar's start
            and stop buttons).
          '';
        };
        barColor = lib.mkOption {
          type = colour;
          default = "#15803d";
          description = "Fill of the flow bar while a dictation goes to the local model.";
        };
        llamaCpp = lib.mkOption {
          type = lib.types.package;
          description = ''
            llama.cpp with its llama-server, build b11852 or later (older
            ones lack Granite Speech's chat template).
          '';
        };
        model = lib.mkOption {
          type = lib.types.nullOr lib.types.str;
          default = null;
          example = "/var/lib/models/granite-speech-5.0-2b-Q4_K_M.gguf";
          description = ''
            Granite Speech 5.0's language model (GGUF); `~/` means the home
            directory. Null: ~/.local/share/wispr-headless/granite-speech-5.0-2b-Q4_K_M.gguf.
          '';
        };
        mmproj = lib.mkOption {
          type = lib.types.nullOr lib.types.str;
          default = null;
          description = ''
            Its audio encoder (GGUF). Null:
            ~/.local/share/wispr-headless/mmproj-model-q4_k-convdw-f32.gguf,
            the F16 encoder quantized with `llama-quantize --tensor-type
            conv_dw=f32 mmproj-model-f16.gguf <out> Q4_K` (318 MB). The
            depthwise convolutions must stay F32: llama.cpp aborts on F16 ones.
          '';
        };
        keywords = lib.mkOption {
          type = lib.types.nullOr lib.types.str;
          default = null;
          example = "~/.config/wispr-flow/keywords.txt";
          description = ''
            File of terms the transcription should spell as listed (names,
            jargon), one per line or separated by commas; up to about 2000.
            Read when each dictation starts, so edits need no rebuild. A long
            list makes the model's context, and its memory, bigger.
          '';
        };
        targetSeconds = lib.mkOption {
          type = lib.types.numbers.between 1 30;
          default = 5;
          description = ''
            The model transcribes a dictation in pieces while it is spoken;
            a piece ends at the first pause after this many seconds. When the
            dictation ends, only the last piece is left to transcribe (about
            a quarter of its length on the Mac). Longer pieces give the model
            more context but leave more to wait for.
          '';
        };
        maxSeconds = lib.mkOption {
          type = lib.types.numbers.between 1 30;
          default = 10;
          description = ''
            A piece without a pause ends here, at its quietest moment (at
            least `targetSeconds`).
          '';
        };
        idleSeconds = lib.mkOption {
          type = lib.types.ints.unsigned;
          default = 0;
          description = ''
            How long the model stays loaded after a dictation, for the next
            one. 0 frees its memory (1.5 GB and more) as soon as the text is
            out.
          '';
        };
        serverArgs = lib.mkOption {
          type = lib.types.listOf lib.types.str;
          default = [ ];
          example = [
            "--flash-attn"
            "on"
          ];
          description = "More arguments for llama-server.";
        };
      };
      hotkeys = lib.mkOption {
        type = lib.types.listOf (lib.types.strMatching "[^[:space:]]+");
        default = [ ];
        example = [
          "wispr:RightAlt"
          "local:RightCtrl"
          "local:toggle:RightCtrl+Space"
          "wispr:toggle:BTN_TASK"
        ];
        description = ''
          Dictation shortcuts that wispr-headless reads from /dev/input
          itself, on any desktop (the users need to be in `users`). A
          `wispr:` binding dictates with Wispr, through its bar's buttons; a
          `local:` one with `localAsr`; without either, `localAsr.backend`
          decides. `Key+Key` holds to talk: it records from the moment it
          goes down, becomes a dictation when held alone for 200 ms, and any
          other key while it is held makes it an ordinary shortcut (Right
          Ctrl+C) and cancels it. `toggle:` starts and stops hands-free; a
          hold that grows into a toggle binding turns hands-free, like
          Wispr's Fn+Space. Keys Wispr's own shortcuts use would start it
          twice. Names are listed in wispr-headless/daemon/src/hotkey/keys.rs;
          any evdev name (KEY_*, BTN_* for mouse buttons) works too, and
          `wispr-headless --hotkey-test` prints the names of the keys pressed.
        '';
      };
      typing = lib.mkOption {
        type = lib.types.enum [
          "auto"
          "ime"
          "keys"
          "paste"
        ];
        default = "auto";
        description = ''
          How wispr-headless types what `localAsr` transcribed (Wispr types
          its own; see wispr-headless/daemon/src/typing/mod.rs). `auto`:
          wispr-ime on GNOME, else key presses through /dev/uinput (Wayland)
          or XTEST (X11), pasting what the layout cannot type.
        '';
      };
    };

    shim = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = if cfg.headless.enable then headlessShim else null;
      defaultText = lib.literalMD "the headless shim when `headless.enable` is set";
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

    # Start it hidden at login, as a user service rather than XDG autostart:
    # home-manager restarts a user service whose unit changed on every switch,
    # and the unit names the package, so a new Wispr build replaces the running
    # one by itself. Restart=on-failure: quitting Wispr from its tray (a clean
    # exit) leaves it stopped. Stopping the unit stops Electron's whole process
    # tree, wispr-headless included.
    home-manager.users = lib.genAttrs cfg.users (_: {
      systemd.user.services.wispr-flow = {
        Unit = {
          Description = "Wispr Flow dictation";
          PartOf = [ "graphical-session.target" ];
          After = [ "graphical-session.target" ];
        };
        Service = {
          ExecStart = "${cfg.package}/bin/wispr-flow --hidden";
          Restart = "on-failure";
          RestartSec = 3;
          TimeoutStopSec = 15;
        };
        Install.WantedBy = [ "graphical-session.target" ];
      };
      # Wispr's own "Open at login" writes this file; a second instance at
      # login would only open its settings window. The service starts it.
      xdg.configFile."autostart/wispr-flow.desktop" = {
        force = true;
        text = ''
          [Desktop Entry]
          Type=Application
          Name=Wispr Flow
          Hidden=true
        '';
      };
    });
  };
}
