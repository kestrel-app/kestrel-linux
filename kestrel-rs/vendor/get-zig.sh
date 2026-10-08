#!/usr/bin/env bash
# Fetch the zig used to cross-link the release build.
#
#   vendor/get-zig.sh          installs into vendor/toolchain/zig/
#
# zig is what pins the glibc floor. Linking on a modern distribution records
# that distribution's glibc as a hard requirement - a build on Debian 13
# demands GLIBC_2.39 and refuses to start anywhere older - and zig carries
# stubs for older versions, so building through it puts the floor where we
# choose regardless of the build machine.
#
# This exists because that used to be somebody's job. ffmpeg and nasm are built
# from source on first use; zig was expected to be already there, unpacked by
# hand into a gitignored directory. On any machine where nobody had done that -
# a fresh clone, a CI runner - the release build quietly fell back to the host
# linker and produced an archive that only ran on machines like the one that
# built it. Fetching it is the difference between a build that is reproducible
# and one that happens to work where it was written.
#
# Nothing is installed system-wide. If zig is already on PATH, build.sh uses
# that instead and this is never called.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
DEST="$HERE/toolchain/zig"

ZIG_VERSION="${ZIG_VERSION:-0.15.2}"
ARCH="$(uname -m)"

# The published SHA-256 for the tarball below. A downloaded toolchain that
# links every shipped binary is worth verifying: without this, whatever the
# network hands back gets to compile the release.
case "$ARCH-$ZIG_VERSION" in
    x86_64-0.15.2)
        SHA256=02aa270f183da276e5b5920b1dac44a63f1a49e55050ebde3aecc9eb82f93239 ;;
    *)
        echo "get-zig.sh has no checksum for zig $ZIG_VERSION on $ARCH." >&2
        echo "Add one from https://ziglang.org/download/index.json, or install" >&2
        echo "zig on PATH and build.sh will use that instead." >&2
        exit 1 ;;
esac

if [[ -x "$DEST/zig" ]]; then
    echo "==> zig $("$DEST/zig" version) already vendored"
    exit 0
fi

# Note the order: <arch>-<os>, which is how the artefacts have been named since
# 0.15. The older zig-linux-x86_64-* form 404s.
NAME="zig-$ARCH-linux-$ZIG_VERSION"
URL="https://ziglang.org/download/$ZIG_VERSION/$NAME.tar.xz"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "==> downloading zig $ZIG_VERSION ($ARCH)"
curl -sSLo "$tmp/zig.tar.xz" "$URL"

echo "==> verifying"
echo "$SHA256  $tmp/zig.tar.xz" | sha256sum -c - >/dev/null

echo "==> unpacking into $DEST"
mkdir -p "$HERE/toolchain"
tar -xJf "$tmp/zig.tar.xz" -C "$tmp"
rm -rf "$DEST"
mv "$tmp/$NAME" "$DEST"

echo "==> zig $("$DEST/zig" version) ready"
