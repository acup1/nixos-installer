#!/bin/sh
set -eu
umask 077

# Checked against Cargo.toml and the Git tag by the release workflow.
release='v0.3.3'
case "$(uname -s)" in
    Linux) os=linux ;;
    Darwin) os=darwin ;;
    *) echo "This installer supports Linux and macOS." >&2; exit 1 ;;
esac
case "$(uname -m)" in
    arm64|aarch64) arch=aarch64 ;;
    x86_64|amd64) arch=x86_64 ;;
    *) echo "There is no installer binary for $(uname -m)." >&2; exit 1 ;;
esac

tmp=$(mktemp -d "${TMPDIR:-/tmp}/nixos-installer.XXXXXXXX")
trap 'rm -rf "$tmp"' EXIT
trap 'exit 1' HUP INT TERM
fetch() {
    curl --fail --silent --show-error --location --proto '=https' --proto-redir '=https' --tlsv1.2 "$1" --output "$2"
}

asset="nixos-installer-$arch-$os"
base="https://github.com/acup1/nixos-installer/releases/download/$release"
fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS"
fetch "$base/$asset" "$tmp/$asset"
expected=$(awk -v name="$asset" '$2 == name && NF == 2 {print $1; n++} END {if (n != 1) exit 1}' "$tmp/SHA256SUMS") || {
    echo "Missing or ambiguous SHA-256 for $asset." >&2; exit 1
}
if command -v sha256sum >/dev/null 2>&1; then
    digest=$(sha256sum < "$tmp/$asset")
else
    digest=$(shasum -a 256 < "$tmp/$asset")
fi
if [ "${digest%% *}" != "$expected" ]; then
    echo "SHA-256 mismatch for $asset." >&2; exit 1
fi
chmod 700 "$tmp/$asset"

# Keep the script reader on the pipe. Only the interactive child uses the terminal.
interactive() {
    if [ -t 0 ]; then "$@"; else "$@" </dev/tty; fi
}
find_nix() {
    if command -v nix >/dev/null 2>&1; then return 0; fi
    for profile in /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh "$HOME/.nix-profile/etc/profile.d/nix.sh"; do
        if [ -r "$profile" ]; then
            set +u
            . "$profile"
            set -u
            if command -v nix >/dev/null 2>&1; then return 0; fi
        fi
    done
    return 1
}
if ! find_nix; then
    if [ "$os" = linux ] && { [ ! -d /run/systemd/system ] || [ -e /sys/fs/selinux/enforce ]; }; then
        fetch https://artifacts.nixos.org/nix-installer "$tmp/install-nix"
        if [ -d /run/systemd/system ]; then
            interactive sh "$tmp/install-nix" install linux
        else
            interactive sh "$tmp/install-nix" install linux --init none
        fi
    else
        fetch https://nixos.org/nix/install "$tmp/install-nix"
        interactive sh "$tmp/install-nix" --daemon
    fi
    find_nix || { echo "The official installer finished but nix is unavailable." >&2; exit 1; }
fi
case "${1:-}" in
    --help|--version|-h|-V) "$tmp/$asset" "$@" ;;
    *) interactive "$tmp/$asset" "$@" ;;
esac
