# Queue, broker and Electron checks run inside the existing KDE VM template.
# Package builds only compile the test executables; they never execute them.
{
  pkgs,
  apiSource ? null,
  bundles ? [ ],
}:
let
  daemon = (pkgs.callPackage ../../wispr-headless/daemon { }).overrideAttrs (old: {
    doCheck = false;
    postBuild = (old.postBuild or "") + ''
      cargo test --offline --locked --profile release --no-run \
        --target ${pkgs.stdenv.hostPlatform.rust.rustcTarget} \
        --message-format=json > "$NIX_BUILD_TOP/test-artifacts.json"
    '';
    postInstall = (old.postInstall or "") + ''
      ${pkgs.python3}/bin/python3 - "$NIX_BUILD_TOP/test-artifacts.json" "$out" <<'PY'
      import json, pathlib, shutil, sys
      out = pathlib.Path(sys.argv[2]) / "libexec" / "tests"
      out.mkdir(parents=True, exist_ok=True)
      for line in pathlib.Path(sys.argv[1]).read_text().splitlines():
          artifact = json.loads(line)
          if artifact.get("reason") == "compiler-artifact" and artifact.get("profile", {}).get("test") and artifact.get("executable"):
              source = pathlib.Path(artifact["executable"])
              shutil.copy2(source, out / source.name)
      PY
    '';
  });
  # Use the same immutable API revision as the daemon's SDK.
  lock = builtins.fromTOML (builtins.readFile ../../wispr-headless/daemon/Cargo.lock);
  sdk = builtins.head (builtins.filter (p: p.name == "dictation-api") lock.package);
  api =
    if apiSource != null then
      pkgs.lib.fileset.toSource {
        root = apiSource;
        fileset = pkgs.lib.fileset.gitTracked apiSource;
      }
    else
      builtins.fetchTree {
        type = "github";
        owner = "sirati";
        repo = "dictation-api";
        rev = builtins.elemAt (builtins.split "#" sdk.source) 2;
      };
  broker = pkgs.rustPlatform.buildRustPackage {
    pname = "dictation-broker-vm-fixture";
    version = "0.1.0";
    src = api;
    cargoLock.lockFile = "${api}/Cargo.lock";
    cargoBuildFlags = [
      "-p"
      "dictation-broker"
    ];
    doCheck = false;
    postBuild = ''
      cargo test --offline --locked --profile release --no-run \
        --target ${pkgs.stdenv.hostPlatform.rust.rustcTarget} \
        -p dictation-broker -p dictation-api -p dictation-protocol \
        --message-format=json > "$NIX_BUILD_TOP/test-artifacts.json"
    '';
    postInstall = ''
      ${pkgs.python3}/bin/python3 - "$NIX_BUILD_TOP/test-artifacts.json" "$out" <<'PY'
      import json, pathlib, shutil, sys
      out = pathlib.Path(sys.argv[2]) / "libexec" / "tests"
      out.mkdir(parents=True, exist_ok=True)
      for line in pathlib.Path(sys.argv[1]).read_text().splitlines():
          artifact = json.loads(line)
          if artifact.get("reason") == "compiler-artifact" and artifact.get("profile", {}).get("test") and artifact.get("executable"):
              source = pathlib.Path(artifact["executable"])
              shutil.copy2(source, out / source.name)
      PY
    '';
  };
  fixtures = pkgs.lib.fileset.toSource {
    root = ../../wispr-headless;
    fileset = pkgs.lib.fileset.unions [
      ../../wispr-headless/shim.js
      ../../wispr-headless/tests
    ];
  };
  checks = pkgs.writeShellScriptBin "wispr-queue-vm-checks" ''
    set -euo pipefail
    export PATH=${
      pkgs.lib.makeBinPath [
        pkgs.bash
        pkgs.coreutils
        pkgs.nodejs
        pkgs.electron_42
        pkgs.xvfb-run
        pkgs.xdotool
        pkgs.procps
        pkgs.dbus
        pkgs.gnused
      ]
    }:$PATH
    export WISPR_HEADLESS_BIN=${daemon}/bin/wispr-headless
    export DICTATION_BROKER_EXE=${broker}/bin/dictation-broker
    export WISPR_TEST_BUNDLES=${pkgs.lib.escapeShellArg (pkgs.lib.concatStringsSep ":" (map toString bundles))}
    for executable in ${broker}/libexec/tests/*; do
      "$executable"
    done
    for executable in ${daemon}/libexec/tests/wispr_headless-*; do
      "$executable"
    done
    for executable in ${daemon}/libexec/tests/daemon-*; do
      "$executable" --include-ignored
    done
    node --test ${fixtures}/tests/helper-routing.cjs
    bash ${fixtures}/tests/electron/run.sh "$WISPR_HEADLESS_BIN"
  '';
in
import ./default.nix {
  inherit pkgs daemon;
  extraPackages = [ checks ];
  extraTestScript = ''

    with subtest("queue, shared-model, broker, Retry and stacked native bubbles"):
        try:
            print(as_alice("wispr-queue-vm-checks > /tmp/queue-checks.log 2>&1", timeout=300))
        finally:
            print(read("/tmp/queue-checks.log"))
            machine.copy_from_vm("/tmp/queue-checks.log")
  '';
}
