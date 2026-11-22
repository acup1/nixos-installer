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
if [ "$#" -eq 0 ]; then
    test -t 0 || exit 2
    echo interactive-binary-on-tty
else
    echo "$asset $1"
fi
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
# A real controlling terminal catches sh continuing to read script commands from /dev/tty.
python3 - <<'PYTHON'
import os, pty, select, signal, time
pid, fd = pty.fork()
if pid == 0:
    os.execv("/bin/sh", ["sh", "-c", "cat bootstrap.sh | sh"])
data = bytearray()
try:
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        if select.select([fd], [], [], 0.25)[0]:
            try:
                chunk = os.read(fd, 4096)
            except OSError as error:
                if error.errno != 5:  # Linux ptys report EIO at end of file.
                    raise
                break
            if not chunk:
                break
            data.extend(chunk)
    else:
        os.kill(pid, signal.SIGTERM)
        raise RuntimeError("piped launcher failed to exit")
finally:
    os.close(fd)
    _, status = os.waitpid(pid, 0)
assert os.waitstatus_to_exitcode(status) == 0, data
assert b"interactive-binary-on-tty" in data, data
PYTHON
echo 'corrupted payload' >> "$fixture/binary"
if sh bootstrap.sh --version; then exit 1; fi
machine=s390x
if sh bootstrap.sh --version; then exit 1; fi
