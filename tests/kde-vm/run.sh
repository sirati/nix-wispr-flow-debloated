#!/usr/bin/env bash
# Build and run the KDE VM test with the repo's pinned nixpkgs (from
# flake.lock). Needs /dev/kvm.
#
#   tests/kde-vm/run.sh        # run the test
#   tests/kde-vm/run.sh -i     # interactive driver (python REPL)
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
sel='test'
[[ "${1:-}" == "-i" ]] && sel='test.driverInteractive'
exec nix build -L --impure --no-link --print-out-paths --expr "
  let
    lock = builtins.fromJSON (builtins.readFile $repo/flake.lock);
    pin = name: builtins.fetchTree lock.nodes.\${name}.locked;
    pkgs = import (pin lock.nodes.root.inputs.nixpkgs) { system = \"x86_64-linux\"; };
    test = import $here/default.nix { inherit pkgs; };
  in $sel"
