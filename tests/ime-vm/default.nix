# GNOME-on-Wayland + fcitx5 VM test harness for the wispr-ime work.
#
#   import ./tests/ime-vm { inherit pkgs; imVariant = "wayland-text-input"; extraTestScript = "..."; }
#
# Returns a `pkgs.testers.runNixOSTest` derivation (one machine, "machine").
# See run.sh next to this file for how to build it with the repo's pinned nixpkgs.
#
# Arguments
#   imVariant        "gtk-module" | "wayland-text-input"
#                      gtk-module: GTK_IM_MODULE=fcitx QT_IM_MODULE=fcitx XMODIFIERS=@im=fcitx
#                                  session-wide (the usual NixOS setup).
#                      wayland-text-input: only XMODIFIERS=@im=fcitx; apps talk text-input-v3 to
#                                  mutter, mutter talks the IBus protocol to fcitx5.
#   extraModules     extra NixOS modules appended to the machine's modules.
#   extraTestScript  Python appended after the baseline checks; all helpers below are in scope.
#   name             test name (also used for the derivation name).
#   fcitxUnitExtra   attrset merged into systemd.user.services.fcitx5 (convenience for experiments;
#                    anything else can go through extraModules).
#   fcitxArgs        arguments of the fcitx5 ExecStart (default "-r", as in wispr-ime.nix; e.g.
#                    "-r --verbose ibus=5" for the IBus frontend's debug log).
{
  pkgs,
  extraModules ? [ ],
  extraTestScript ? "",
  imVariant ? "wayland-text-input",
  name ? "ime-vm-${imVariant}",
  fcitxUnitExtra ? { },
  fcitxArgs ? "-r",
  ...
}:
assert pkgs.lib.assertOneOf "imVariant" imVariant [
  "gtk-module"
  "wayland-text-input"
];
let
  lib = pkgs.lib;

  imModule =
    { config, ... }:
    {
      i18n.inputMethod = {
        enable = true;
        type = "fcitx5";
        fcitx5 = {
          waylandFrontend = true;
          ignoreUserConfig = true; # exports SKIP_FCITX_USER_PATH=1 session-wide (on purpose)
          addons = [ pkgs.fcitx5-mozc ];
          settings.inputMethod = {
            "Groups/0" = {
              Name = "Default";
              "Default Layout" = "gb";
              DefaultIM = "keyboard-gb-intl";
            };
            "Groups/0/Items/0".Name = "keyboard-gb-intl";
            "Groups/0/Items/1".Name = "mozc";
            "Groups/0/Items/2".Name = "keyboard-us";
          };
          settings.globalOptions."Hotkey/TriggerKeys"."0" = "Super+space";
        };
      };

      # Session-wide environment, as NixOS sets it (pam_env => GDM session => systemd user manager).
      environment.sessionVariables =
        {
          XMODIFIERS = "@im=fcitx";
        }
        // lib.optionalAttrs (imVariant == "gtk-module") {
          GTK_IM_MODULE = "fcitx";
          QT_IM_MODULE = "fcitx";
        };

      # As in wispr-ime.nix: fcitx5 runs as a user service ...
      systemd.user.services.fcitx5 = lib.mkMerge [
        {
          description = "Fcitx5 input method";
          wantedBy = [ "graphical-session.target" ];
          after = [ "graphical-session.target" ];
          partOf = [ "graphical-session.target" ];
          serviceConfig = {
            ExecStart = "${config.i18n.inputMethod.package}/bin/fcitx5 ${fcitxArgs}";
            Restart = "on-failure";
            RestartSec = 2;
          };
        }
        fcitxUnitExtra
      ];

      # ... and the XDG autostart entry (/etc/xdg/autostart/org.fcitx.Fcitx5.desktop) is hidden for alice.
      systemd.tmpfiles.settings."10-hide-fcitx5-autostart" = {
        "/home/alice/.config".d = {
          user = "alice";
          group = "users";
          mode = "0700";
        };
        "/home/alice/.config/autostart".d = {
          user = "alice";
          group = "users";
          mode = "0755";
        };
        "/home/alice/.config/autostart/org.fcitx.Fcitx5.desktop"."f+" = {
          user = "alice";
          group = "users";
          mode = "0644";
          # real newlines; the tmpfiles module escapes them itself
          argument = "[Desktop Entry]\nType=Application\nName=Fcitx 5\nHidden=true\n";
        };
      };
    };

  gnomeModule =
    { ... }:
    {
      users.users.alice = {
        isNormalUser = true;
        description = "Alice Foobar";
        password = "foobar";
        uid = 1000;
      };

      services.displayManager.gdm = {
        enable = true;
        autoSuspend = false;
      };
      services.displayManager.autoLogin = {
        enable = true;
        user = "alice";
      };
      services.desktopManager.gnome.enable = true;

      # Shell.Eval needs unsafe mode; the test uses it to learn which window has focus.
      systemd.user.services."org.gnome.Shell@" = {
        serviceConfig.ExecStart = [
          ""
          "${pkgs.gnome-shell}/bin/gnome-shell --unsafe-mode"
        ];
      };

      environment.systemPackages = [
        pkgs.gnome-console # kgx, GTK4
        pkgs.gnome-terminal # GTK3
      ];

      # GNOME has a single xkb input source, `gb`, and gives Super+Space up so fcitx5 gets it.
      programs.dconf.profiles.user.databases = [
        {
          settings = {
            "org/gnome/desktop/input-sources".sources = [ (lib.gvariant.mkTuple [ "xkb" "gb" ]) ];
            "org/gnome/desktop/wm/keybindings" = {
              switch-input-source = [ "XF86Keyboard" ];
              switch-input-source-backward = [ "<Shift>XF86Keyboard" ];
            };
            "org/gnome/desktop/session".idle-delay = lib.gvariant.mkUint32 0;
            "org/gnome/desktop/screensaver".lock-enabled = false;
            "org/gnome/desktop/interface".enable-animations = false;
            "org/gnome/shell".welcome-dialog-last-shown-version = "999";
          };
        }
      ];

      # Small guest, no network, no sound.
      virtualisation = {
        memorySize = 4096;
        cores = 3;
        vlans = [ ];
        resolution = {
          x = 1024;
          y = 768;
        };
      };
      # Standard VGA with an EDID whose preferred mode is 1024x768 (mutter picks the preferred mode).
      virtualisation.qemu.options = [
        "-vga none"
        "-device VGA,edid=on,xres=1024,yres=768"
      ];
    };

  prelude = builtins.readFile ./prelude.py;
in
pkgs.testers.runNixOSTest {
  inherit name;
  skipLint = true;
  skipTypeCheck = true;
  nodes.machine = {
    imports = [
      gnomeModule
      imModule
    ] ++ extraModules;
  };

  testScript =
    ''
      IM_VARIANT = "${imVariant}"
      PATH_KGX = "${pkgs.gnome-console}/bin/kgx"
      PATH_GNOME_TERMINAL = "${pkgs.gnome-terminal}/bin/gnome-terminal"
      PATH_SH = "${pkgs.bash}/bin/sh"
      PATH_CAT = "${pkgs.coreutils}/bin/cat"
    ''
    + prelude
    + ''

      # ---------------------------------------------------------------- baseline checks
      start_all()
      boot_to_desktop()

      with subtest("baseline: session environment matches imVariant"):
          env = session_environment()
          print(env)
          assert "XMODIFIERS=@im=fcitx" in env
          assert "SKIP_FCITX_USER_PATH=1" in env, "ignoreUserConfig should export SKIP_FCITX_USER_PATH"
          if IM_VARIANT == "gtk-module":
              assert "GTK_IM_MODULE=fcitx" in env and "QT_IM_MODULE=fcitx" in env
          else:
              assert "GTK_IM_MODULE" not in env and "QT_IM_MODULE" not in env

      with subtest("baseline: fcitx5 runs exactly once, as the user service"):
          machine.wait_until_succeeds(
              "runuser -u alice -- env " + SESSION_ENV + " systemctl --user is-active fcitx5.service",
              timeout=120,
          )
          machine.wait_until_succeeds(
              "runuser -u alice -- env " + SESSION_ENV + " busctl --user list | grep -q org.fcitx.Fcitx5",
              timeout=60,
          )
          # comm is ".fcitx5-wrapped" for the nixpkgs wrapper, hence no -x
          n = machine.succeed("pgrep -u alice -c fcitx5 || true").strip()
          assert n == "1", f"expected one fcitx5 process, got {n}"
          print("fcitx5 current IM:", fcitx_current_im())

      with subtest("baseline: display size"):
          print("monitor mode:", shell_eval("global.display.get_monitor_geometry(0).width + 'x' + global.display.get_monitor_geometry(0).height"))

      ime_state("after baseline")
    ''
    + extraTestScript;
}
