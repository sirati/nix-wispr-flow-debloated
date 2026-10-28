# KDE Plasma on Wayland in a VM: wispr-headless's own dictation shortcut
# from /dev/input and its typing through a uinput keyboard, with a German
# layout set in KDE. The keyboard is a stand-in (stand-in-keyboard.py, a
# uinput device), and so are a mouse button and a speakerphone's call
# button that only shows in raw HID reports (stand-in-buttons.py). The text
# goes into a native Wayland GTK window (capture-text.py) and Konsole. Runs
# without Wispr: the daemon's --hotkey-test and --type-test modes exercise
# the same code.
#
#   tests/kde-vm/run.sh        # needs /dev/kvm
#   tests/kde-vm/run.sh -i     # interactive driver
{
  pkgs,
  daemon ? (pkgs.callPackage ../../wispr-headless/daemon { }).overrideAttrs {
    doCheck = false;
  },
  extraPackages ? [ ],
  extraTestScript ? "",
}:
let
  standIn = pkgs.writers.writePython3Bin "stand-in-keyboard" {
    libraries = [ pkgs.python3Packages.evdev ];
  } (builtins.readFile ./stand-in-keyboard.py);

  standInButtons = pkgs.writers.writePython3Bin "stand-in-buttons" {
    libraries = [ pkgs.python3Packages.evdev ];
    flakeIgnore = [ "E501" ];
  } (builtins.readFile ./stand-in-buttons.py);

  captureText = pkgs.stdenv.mkDerivation {
    name = "capture-text";
    dontUnpack = true;
    nativeBuildInputs = [
      pkgs.wrapGAppsHook3
      pkgs.gobject-introspection
    ];
    buildInputs = [
      pkgs.gtk3
      (pkgs.python3.withPackages (p: [ p.pygobject3 ]))
    ];
    installPhase = "install -Dm755 ${./capture-text.py} $out/bin/capture-text";
  };
in
pkgs.testers.runNixOSTest {
  name = "kde-vm-wispr-headless";
  skipLint = true;
  skipTypeCheck = true;

  nodes.machine =
    { ... }:
    {
      users.users.alice = {
        isNormalUser = true;
        password = "foobar";
        uid = 1000;
        # /dev/input and /dev/uinput, as programs.wisprFlow.users grants.
        extraGroups = [ "input" ];
      };
      services.displayManager.plasma-login-manager.enable = true;
      services.desktopManager.plasma6.enable = true;
      services.displayManager.autoLogin = {
        enable = true;
        user = "alice";
      };

      # Wispr's udev rule, for the helper's (and our) uinput keyboard.
      # uhid for the stand-in speaker; its raw HID node readable as
      # programs.wisprFlow makes it for a `hid:1209:0001/...` hotkey.
      boot.kernelModules = [
        "uinput"
        "uhid"
      ];
      services.udev.extraRules = ''
        KERNEL=="uinput", SUBSYSTEM=="misc", GROUP="input", MODE="0660"
        SUBSYSTEM=="hidraw", KERNELS=="*:1209:0001.*", GROUP="input", MODE="0660"
      '';

      # A German layout, set the way KDE's settings save it.
      systemd.tmpfiles.settings."10-kxkbrc" = {
        "/home/alice/.config".d = {
          user = "alice";
          group = "users";
          mode = "0700";
        };
        "/home/alice/.config/kxkbrc"."f+" = {
          user = "alice";
          group = "users";
          mode = "0644";
          argument = "[Layout]\nLayoutList=de\nUse=true\n";
        };
      };

      environment.systemPackages = [
        daemon
        standIn
        standInButtons
        captureText
        pkgs.wl-clipboard
      ]
      ++ extraPackages;

      virtualisation = {
        memorySize = 4096;
        cores = 4;
        vlans = [ ];
      };
    };

  testScript = ''
    DAEMON = "${daemon}/bin/wispr-headless"
    WL_COPY = "${pkgs.wl-clipboard}/bin/wl-copy"
  ''
  + builtins.readFile ./test.py
  + extraTestScript;
}
