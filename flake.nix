{
  description = "Wispr Flow for NixOS and macOS without Chromium running in the background";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";

    # Unofficial Linux port of the proprietary Wispr Flow dictation app (not
    # affiliated with Wispr). wispr-flow.nix reads it as a source tree; its
    # installer pin and patch scripts must match the rev, so bump the rev
    # deliberately and rebuild before switching.
    wispr-flow-linux = {
      url = "github:wispr-flow-linux/wispr-flow-linux/c7b53b995c2c09e71c6e972bdd62ac19a7557551";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      wispr-flow-linux,
    }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
    in
    {
      nixosModules = {
        # programs.wisprFlow: the Linux port, with the headless shim.
        wispr-flow = {
          _file = ./wispr-flow.nix;
          imports = [ (import ./wispr-flow.nix { inherit wispr-flow-linux; }) ];
        };
        # programs.wisprIme: dictation through fcitx5 instead of the clipboard.
        wispr-ime = ./wispr-ime.nix;
        # services.dictationButtons: mouse and headset buttons as a dictation key.
        dictation-buttons = ./dictation-buttons.nix;
        default.imports = [
          self.nixosModules.wispr-flow
          self.nixosModules.wispr-ime
          self.nixosModules.dictation-buttons
        ];
      };

      # The Mac app (see darwin/wispr-flow.nix). Cross
      # compiling needs an Apple SDK toolchain, which the caller provides:
      #   lib.macWisprFlow { native; builders; darwin; daemon; user; localAsr ? null; }
      # with daemon = a cross build of ./wispr-headless/daemon, and
      # optionally signingIdentity, hotkeys and typing.
      lib.macWisprFlow = import ./darwin/wispr-flow.nix;

      # llama.cpp for Granite Speech (see llama-cpp/default.nix), cross-built
      # for the Mac with the same toolchain:
      #   lib.macLlamaCpp { native; stdenv; sdk; }
      # with stdenv = an stdenv whose cc is an aarch64-apple-darwin clang
      # and sdk = the macOS SDK it uses.
      lib.macLlamaCpp =
        {
          native,
          stdenv,
          sdk,
        }:
        native.callPackage ./llama-cpp { inherit stdenv sdk; };

      packages.${system} = {
        wispr-headless = pkgs.callPackage ./wispr-headless/daemon { };
        wispr-ime = pkgs.callPackage ./wispr-ime { };
        wispr-ime-helper = pkgs.callPackage ./wispr-ime/helper.nix { inherit wispr-flow-linux; };
        llama-cpp = pkgs.callPackage ./llama-cpp { };
      };

      checks.${system} = {
        inherit (self.packages.${system}) wispr-headless wispr-ime;
      };
    };
}
