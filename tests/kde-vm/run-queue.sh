#!/usr/bin/env bash
# Build artifacts on the host, execute every check inside the KDE VM template.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
api_arg=""
if [[ -n "${1:-}" ]]; then
  api_arg="apiSource = $(realpath "$1");"
fi
exec nix build -L --impure --no-link --print-out-paths --expr "
  let
    lock = builtins.fromJSON (builtins.readFile $repo/flake.lock);
    pin = name: builtins.fetchTree lock.nodes.\${name}.locked;
    pkgs = import (pin lock.nodes.root.inputs.nixpkgs) { system = \"x86_64-linux\"; };
    bundles = map (path: builtins.path { inherit path; })
      (builtins.filter (path: path != \"\")
        (pkgs.lib.splitString \":\" (builtins.getEnv \"WISPR_TEST_BUNDLES\")));
  in import $here/queue.nix { inherit pkgs bundles; $api_arg }"
