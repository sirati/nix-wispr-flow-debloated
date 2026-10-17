# wispr-ime in the GNOME VM: the real mutter, the real fcitx5, the patched
# Wispr helper, and a stand-in for the voice (a script that sends the helper
# the same PasteText request Wispr's app sends once it has a transcript).
{ pkgs, wisprFlowLinux }:
let
  wisprIme = pkgs.callPackage ../../wispr-ime { };
  helper = pkgs.callPackage ../../wispr-ime/helper.nix { wispr-flow-linux = wisprFlowLinux; };
in
import ./default.nix {
  inherit pkgs;
  name = "ime-vm-wispr-ime";
  imVariant = "wayland-text-input";
  # Same as the laptop (wispr-ime.nix at the repo root): wispr-ime answers GNOME's
  # IBus, so fcitx5's own frontend stays out of its way; --keep and Restart=always
  # bring fcitx5 back after the clean exits it makes on its own.
  fcitxArgs = "-r --keep --disable ibusfrontend";
  fcitxUnitExtra.serviceConfig.Restart = pkgs.lib.mkForce "always";

  extraModules = [
    (
      { ... }:
      {
        # The same shape as the laptop's home-manager services (wispr-ime.nix).
        systemd.user.services.wispr-ime = {
          description = "IBus front for GNOME that hands input to fcitx5";
          wantedBy = [ "graphical-session.target" ];
          # Explicitly after the session too, or systemd finds an ordering
          # cycle through fcitx5 and drops this unit's start job.
          after = [
            "graphical-session.target"
            "fcitx5.service"
          ];
          partOf = [ "fcitx5.service" ];
          environment.WISPR_IME_LOG = "debug";
          serviceConfig = {
            ExecStart = "${wisprIme}/bin/wispr-ime";
            Restart = "on-failure";
            RestartSec = 2;
          };
        };

        # The helper types through /dev/uinput and reads /dev/input.
        boot.kernelModules = [ "uinput" ];
        services.udev.extraRules = ''KERNEL=="uinput", SUBSYSTEM=="misc", GROUP="input", MODE="0660"'';
        users.users.alice.extraGroups = [ "input" ];

        environment.systemPackages = [ pkgs.wl-clipboard ];
      }
    )
  ];

  extraTestScript =
    ''
      HELPER = "${helper}/bin/wispr-flow-linux-helper"
    ''
    + builtins.readFile ./wispr-ime-test.py;
}
