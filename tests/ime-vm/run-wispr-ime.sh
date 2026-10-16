#!/usr/bin/env bash
# Build and run the wispr-ime VM test with the repo's pinned nixpkgs and the
# pinned wispr-flow-linux source (both from flake.lock). Needs /dev/kvm.
#
#   tests/ime-vm/run-wispr-ime.sh        # run the test
#   tests/ime-vm/run-wispr-ime.sh -i     # interactive driver (python REPL)
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
    test = import $here/wispr-ime.nix { inherit pkgs; wisprFlowLinux = pin lock.nodes.root.inputs.wispr-flow-linux; };
  in $sel"
