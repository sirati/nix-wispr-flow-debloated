{
  lib,
  stdenv,
  rustPlatform,
  xvfb,
  setxkbmap,
  xkbcomp,
  dbus,
}:

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
  cargoLock.outputHashes = {
    "dictation-api-0.1.0" = "sha256-+UR44Ui6v8jO2HQPnOsqb8TOzbPxrJQVgwrFYhqjKLM=";
  };

  # Compile here; execute tests only in tests/kde-vm/run-queue.sh. Package
  # builds must not use the developer's live desktop as a test environment.
  # macOS packages are cross-built on Linux too.
  doCheck = false;
  nativeCheckInputs = lib.optionals stdenv.hostPlatform.isLinux [
    xvfb
    setxkbmap
    xkbcomp
    dbus
  ];

  meta = {
    description = "Recorder and speech indicator that stand in for Wispr Flow's hidden windows";
    license = lib.licenses.mit;
    mainProgram = "wispr-headless";
    platforms = lib.platforms.linux ++ [ "aarch64-darwin" ];
  };
}
