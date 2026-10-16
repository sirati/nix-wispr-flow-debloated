{ lib, rustPlatform }:

rustPlatform.buildRustPackage {
  pname = "wispr-ime";
  version = "0.1.0";

  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [
      ./Cargo.toml
      ./Cargo.lock
      ./src
      ./tests
    ];
  };

  cargoLock.lockFile = ./Cargo.lock;

  # The unit tests run here. The end-to-end test needs fcitx5 and dbus-daemon
  # and skips itself without them; the VM test (tests/ime-vm) covers it with
  # the real GNOME.

  meta = {
    description = "Safe-Rust IBus front for GNOME that hands input to fcitx5 and lets dictation commit text";
    license = lib.licenses.mit;
    mainProgram = "wispr-ime";
    platforms = lib.platforms.linux;
  };
}
