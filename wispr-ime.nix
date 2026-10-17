# Dictation through the session's input method.
#
# GNOME talks to its input method over the IBus protocol. fcitx5 can answer it
# with its own IBus frontend, but with `ignoreUserConfig` (SKIP_FCITX_USER_PATH)
# it cannot write the address file mutter reads, so GNOME's own path to fcitx5
# is dead and only programs that load fcitx5's GTK/Qt modules reach it. This
# module runs fcitx5 as a user service without that frontend and puts
# wispr-ime in its place: a safe-Rust daemon that answers GNOME as IBus,
# forwards everything to fcitx5, writes the address file, and offers one call,
# org.wisprime.WisprIme1.Commit, that commits text into the focused field.
# Wispr's helper uses that call instead of the clipboard (see wispr-flow.nix).
#
# Apps must talk to GNOME through its own text channel for any of this to
# reach them, so GTK_IM_MODULE/QT_IM_MODULE are not set while this is on;
# XMODIFIERS stays for XWayland programs, which keep talking to fcitx5 directly.
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.programs.wisprIme;
in
{
  options.programs.wisprIme = {
    enable = lib.mkEnableOption "wispr-ime, the IBus front that lets dictation type through fcitx5";

    user = lib.mkOption {
      type = lib.types.str;
      example = "alice";
      description = "User whose GNOME session runs fcitx5 and wispr-ime.";
    };

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ./wispr-ime { };
      defaultText = lib.literalMD "the crate in `./wispr-ime`";
      description = "The wispr-ime package.";
    };
  };

  config = lib.mkIf cfg.enable {
    home-manager.users.${cfg.user} = {
      # GNOME starts fcitx5 at login as a one-off scope that nothing restarts,
      # and fcitx5 reads its config and add-ons only at start, so a switch
      # changed nothing until it was restarted by hand. Run it as a user
      # service instead: home-manager restarts changed user services on every
      # switch, and the triggers cover the config files (a new add-on changes
      # the ExecStart path). -r takes over from an instance that is already
      # running, such as the one GNOME started before this service existed.
      # Its IBus frontend is off because wispr-ime answers for it.
      #
      # Restart=always and --keep: fcitx5 exits with status 0 when its display
      # connection goes away or it is asked to quit, and Restart=on-failure
      # treats that as success and leaves it dead, which took input methods
      # down on the laptop until someone started it by hand. --keep stops the
      # display-loss exit; Restart=always covers every other clean exit.
      systemd.user.services.fcitx5 = {
        Unit = {
          Description = "Fcitx5 input method";
          PartOf = [ "graphical-session.target" ];
          After = [ "graphical-session.target" ];
          X-Restart-Triggers = [
            config.environment.etc."xdg/fcitx5/config".source
            config.environment.etc."xdg/fcitx5/profile".source
          ];
        };
        Service = {
          ExecStart = "${config.i18n.inputMethod.package}/bin/fcitx5 -r --keep --disable ibusfrontend";
          Restart = "always";
          RestartSec = 2;
        };
        Install.WantedBy = [ "graphical-session.target" ];
      };

      # PartOf: when fcitx5 restarts, its input contexts are gone, so
      # wispr-ime restarts too and GNOME reconnects. It is ordered after the
      # graphical session explicitly: a target orders itself after the units it
      # pulls in unless they already order after it, and without this the
      # chain wispr-ime -> fcitx5 -> graphical-session -> wispr-ime is a cycle
      # that makes systemd drop the start job.
      systemd.user.services.wispr-ime = {
        Unit = {
          Description = "IBus front for GNOME that hands input to fcitx5";
          PartOf = [ "fcitx5.service" ];
          After = [
            "graphical-session.target"
            "fcitx5.service"
          ];
        };
        Service = {
          ExecStart = "${cfg.package}/bin/wispr-ime";
          Restart = "on-failure";
          RestartSec = 2;
        };
        Install.WantedBy = [ "graphical-session.target" ];
      };

      # Hidden: the service above is the only thing that starts fcitx5.
      xdg.configFile."autostart/org.fcitx.Fcitx5.desktop".text = ''
        [Desktop Entry]
        Type=Application
        Name=Fcitx 5
        Hidden=true
      '';
    };
  };
}
