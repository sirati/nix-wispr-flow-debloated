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
    "dictation-api-0.1.0" = "sha256-kJU5cXKYHC7grvzajgwCavFKMKzOio7QjjjnMv4ftVE=";
  };

  # The unit tests and the binary's own tests (a stand-in microphone) run
  # here; the indicator is skipped without a display. Typing through XTEST
  # runs against Xvfb, and the D-Bus client against a private dbus-daemon.
  # The macOS build is cross-compiled on Linux by the caller (see
  # lib.macWisprFlow); its tests run on the Mac instead.
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
