# The port's Linux helper with ./helper-ime-commit.patch: PasteText commits
# through wispr-ime and pastes only when that cannot. Built from the same
# source the port pins, so a new helper release fails here (hash mismatch or
# patch rejects) until the hash and patch are updated.
{
  lib,
  rustPlatform,
  fetchFromGitHub,
  wispr-flow-linux,
}:

let
  pin = lib.removeSuffix "\n" (builtins.readFile "${wispr-flow-linux}/helper-version.txt");
  src = fetchFromGitHub {
    owner = "wispr-flow-linux";
    repo = "helper";
    rev = pin;
    hash = "sha256-6yImRj0KCgoj6TCnIKSY94jF84ZGF2oz4CKpiBuJIJU=";
  };
in
rustPlatform.buildRustPackage {
  pname = "wispr-flow-linux-helper";
  version = "${lib.removePrefix "v" pin}-ime";
  inherit src;
  patches = [ ./helper-ime-commit.patch ];
  cargoLock.lockFile = "${src}/Cargo.lock";
  doCheck = false;
  meta.mainProgram = "wispr-flow-linux-helper";
}
