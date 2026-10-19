{ lib, rustPlatform }:

rustPlatform.buildRustPackage {
  pname = "wispr-headless";
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

  # The unit tests and the binary's own tests (a stand-in microphone) run
  # here; the indicator is skipped without a display. The macOS build is
  # cross-compiled on Linux by the caller (see lib.macWisprFlow); its tests
  # run on the Mac instead.

  meta = {
    description = "Recorder and speech indicator that stand in for Wispr Flow's hidden windows";
    license = lib.licenses.mit;
    mainProgram = "wispr-headless";
    platforms = lib.platforms.linux ++ [ "aarch64-darwin" ];
  };
}
