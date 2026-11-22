#!/bin/sh
# Exercise the release launcher without downloading software or installing Nix.
set -eu
fixture=$(mktemp -d)
trap 'rm -rf "$fixture"' EXIT
export fixture
mkdir -p "$fixture/bin"
cat > "$fixture/bin/uname" <<'EOF'
#!/bin/sh
case "$1" in -s) echo "$system";; -m) echo "$machine";; esac
EOF
cat > "$fixture/bin/curl" <<'EOF'
#!/bin/sh
set -eu
while [ "$1" != "--output" ]; do url=$1; shift; done
case "$url" in
    */SHA256SUMS) cp "$fixture/sums" "$2" ;;
    */"$asset") cp "$fixture/binary" "$2" ;;
    *) echo "Unexpected download: $url" >&2; exit 1 ;;
esac
EOF
cat > "$fixture/binary" <<'EOF'
#!/bin/sh
echo "$asset $1"
EOF
for tool in nix git; do printf '#!/bin/sh\nexit 99\n' > "$fixture/bin/$tool"; done
chmod +x "$fixture"/bin/*
export PATH="$fixture/bin:$PATH" machine system asset
for pair in 'Linux aarch64 aarch64-linux' 'Darwin arm64 aarch64-darwin' \
    'Linux x86_64 x86_64-linux' 'Darwin x86_64 x86_64-darwin'; do
    set -- $pair
    system=$1 machine=$2 asset="nixos-installer-$3"
    digest=$(shasum -a 256 < "$fixture/binary")
    printf '%s  %s\n' "${digest%% *}" "$asset" > "$fixture/sums"
    test "$(sh bootstrap.sh --version)" = "$asset --version"
done
echo 'corrupted payload' >> "$fixture/binary"
if sh bootstrap.sh --version; then exit 1; fi
machine=s390x
if sh bootstrap.sh --version; then exit 1; fi
