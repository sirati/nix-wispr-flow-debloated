#!/bin/bash
# Installs the Nix-built Wispr Flow (darwin/wispr-flow.nix) into /Applications
# and signs it there. Run with sudo after copying the bundle closure from the build machine.
set -euo pipefail
export PATH=/usr/bin:/bin:/usr/sbin:/sbin
bundle='@bundle@'
version='@version@'
profile=/nix/var/nix/profiles/mac-wispr-flow
user=alice
user_home=/Users/alice
target='/Applications/Wispr Flow.app'
source_app="$bundle/Applications/Wispr Flow.app"
keychain=/Library/Keychains/System.keychain
identity='Wispr Flow local code signing (Nix)'
state_dir='/Library/Application Support/nix-wispr-flow'
backups="$user_home/.local/setup/app-backups"

if [[ $(id -u) != 0 ]]; then
    echo "Run this activation with sudo." >&2
    exit 1
fi
uid=$(id -u "$user")

nix_env=/nix/var/nix/profiles/default/bin/nix-env
[[ -x "$nix_env" ]] || nix_env=/run/current-system/sw/bin/nix-env
if [[ ! -x "$nix_env" ]]; then
    echo "Cannot find the installed nix-env executable." >&2
    exit 1
fi
# --set selects an existing store object. It does not evaluate or build a package.
"$nix_env" --profile "$profile" --set "$bundle"

# A local code-signing certificate, created once. Its private key exists only
# in this Mac's System keychain, where only root and codesign can use it. It
# needs no trust setting: the signature pins this certificate, which is what
# keeps macOS's permissions for the app across rebuilds.
if ! security find-certificate -c "$identity" "$keychain" >/dev/null 2>&1; then
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    cat > "$tmp/cert.cnf" <<EOF
[req]
distinguished_name=dn
x509_extensions=ext
prompt=no
[dn]
CN=$identity
[ext]
basicConstraints=critical,CA:false
keyUsage=critical,digitalSignature
extendedKeyUsage=critical,codeSigning
EOF
    pass=$(openssl rand -hex 16)
    openssl req -x509 -newkey rsa:3072 -nodes -keyout "$tmp/key.pem" -out "$tmp/cert.pem" \
        -days 36500 -config "$tmp/cert.cnf" 2>/dev/null
    openssl pkcs12 -export -inkey "$tmp/key.pem" -in "$tmp/cert.pem" -out "$tmp/id.p12" \
        -passout "pass:$pass" -name "$identity"
    security import "$tmp/id.p12" -k "$keychain" -P "$pass" -T /usr/bin/codesign >/dev/null
    rm -rf "$tmp"
    trap - EXIT
    echo "Created the code-signing certificate \"$identity\" in the System keychain."
fi

# The app's store path changes only with Wispr's version; the shim and
# wispr-headless come from the profile set above, so a restart picks them up.
install -d -m 755 "$state_dir"
app_store=$(readlink -f "$source_app")
installed=$(cat "$state_dir/installed" 2>/dev/null || true)
same_app=0
if [[ "$installed" == "$app_store" && -d "$target" ]] && codesign --verify "$target" 2>/dev/null; then
    same_app=1
fi

user_run() { launchctl asuser "$uid" sudo -u "$user" "$@"; }
running() { pgrep -x -u "$uid" 'Wispr Flow' >/dev/null; }

was_running=0
if running; then
    was_running=1
    user_run osascript -e 'quit app id "com.electron.wispr-flow"' >/dev/null 2>&1 || true
    for _ in $(seq 1 50); do running || break; sleep 0.2; done
    if running; then
        pkill -x -u "$uid" 'Wispr Flow' || true
        sleep 1
    fi
fi

if [[ "$same_app" == 0 ]]; then
    # Keep Wispr's own build the first time it is replaced. (Not `| grep -q`:
    # with pipefail, codesign's SIGPIPE would make the test fail.)
    signature=$( { codesign -dv "$target" 2>&1 || true; } )
    if [[ -d "$target" && "$signature" == *'Authority=Developer ID Application: Wispr AI'* ]]; then
        old=$(defaults read "$target/Contents/Info" CFBundleShortVersionString 2>/dev/null || echo unknown)
        install -d -m 755 -o "$user" -g staff "$user_home/.local" "$user_home/.local/setup" "$backups"
        dest="$backups/Wispr Flow-$old.app"
        if [[ -e "$dest" ]]; then
            rm -rf "$target"
        else
            mv "$target" "$dest"
            echo "Moved Wispr's own Wispr Flow $old to $dest."
        fi
    fi

    # A pending Squirrel update would replace this build when Wispr quits.
    rm -f "$user_home/Library/Caches/com.electron.wispr-flow.ShipIt/ShipItState.plist"

    new='/Applications/.Wispr Flow.app.nix-new'
    rm -rf "$new"
    ditto "$source_app" "$new"
    chmod -R u+w "$new"
    chown -R "$user":admin "$new"
    # Only the outer bundle: Electron's and Wispr's nested code keep Wispr's
    # signatures, which still match.
    codesign --force --sign "$identity" --keychain "$keychain" "$new"
    codesign --verify "$new"
    rm -rf "$target"
    mv "$new" "$target"
    echo "$app_store" > "$state_dir/installed"
    echo "Installed Wispr Flow $version (headless, Nix-pinned) in $target."
else
    echo "Wispr Flow $version is already installed; restarting it with the new shim."
fi

if [[ "$was_running" == 1 ]] || launchctl print "gui/$uid" >/dev/null 2>&1; then
    user_run open -g "$target" || echo "Could not start Wispr Flow; open it from /Applications."
fi
