#!/usr/bin/env bash
# Build wrapper: sets up the vendored ffmpeg and the couple of environment
# variables its bindings need, then hands off to cargo.
#
#   ./build.sh build --release
#   ./build.sh test
#   ./build.sh run -- 192.0.2.242 --stream
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"

# Build ffmpeg on first use (~2 minutes; cached afterwards).
# Rebuild ffmpeg when it is missing, or when it was built for a different
# glibc floor than this build wants. Testing only for the directory meant a
# prefix built without zig - against the build machine's glibc - was reused by
# a release build that targets an older one, and the link failed a job later
# with undefined references to symbols the ffmpeg libraries had been happy to
# import.
ffmpeg_stamp="$HERE/vendor/prefix/.built-for-glibc"
want_glibc="${GLIBC_TARGET:-2.28}"
if [[ ! -d "$HERE/vendor/prefix/lib" ]]; then
    echo "==> vendored ffmpeg not found, building it first"
    "$HERE/vendor/build-ffmpeg.sh"
elif [[ ! -f "$ffmpeg_stamp" ]] || [[ "$(cat "$ffmpeg_stamp")" != "$want_glibc" ]]; then
    echo "==> vendored ffmpeg was built for glibc $(cat "$ffmpeg_stamp" 2>/dev/null || echo 'unknown')," \
         "rebuilding for $want_glibc"
    # Empty it through the link rather than removing the path: ci-cache.sh
    # makes vendor/prefix a symlink into a cache outside the build directory,
    # and `rm -rf` on a symlink removes the link and leaves the stale contents
    # behind - so the rebuild would land in the build directory, the cache
    # would keep the wrong ffmpeg, and every job would rebuild it again.
    find "$HERE/vendor/prefix/" -mindepth 1 -maxdepth 1 -exec rm -rf {} + 2>/dev/null || true
    "$HERE/vendor/build-ffmpeg.sh"
fi

export PKG_CONFIG_PATH="$HERE/vendor/prefix/lib/pkgconfig"

# bindgen drives libclang, which looks for its own builtin headers. On a machine
# with no clang package installed those are missing and every header that pulls
# in limits.h fails; gcc's include directory supplies them.
#
# /usr/include has to be named explicitly as well: once a --target is passed for
# the cross-linked release build, clang stops assuming the host's system headers
# and gcc's stdint.h can no longer find the one it includes next.
if [[ -z "${BINDGEN_EXTRA_CLANG_ARGS:-}" ]]; then
    gcc_include="$(ls -d /usr/lib/gcc/*/*/include 2>/dev/null | sort -V | tail -1 || true)"
    args=""
    [[ -n "$gcc_include" ]] && args="-I$gcc_include"
    [[ -d /usr/include ]] && args="$args -I/usr/include"
    # Debian splits libc headers by architecture; clang only guesses that
    # directory correctly for its own default triple. Pick by machine — this box
    # carries cross headers for a dozen architectures and the first alphabetically
    # is aarch64.
    multiarch="/usr/include/$(uname -m)-linux-gnu"
    [[ -d "$multiarch" ]] || multiarch=""
    [[ -n "$multiarch" ]] && args="$args -I$multiarch"
    [[ -n "$args" ]] && export BINDGEN_EXTRA_CLANG_ARGS="$args"
fi

# The glibc floor of the shipped binary.
#
# Linking on a modern distro records that distro's glibc as a hard requirement —
# a build on Debian 13 demanded GLIBC_2.39 and refused to start anywhere older,
# which breaks "copy the archive to any Linux machine". zig carries glibc stubs
# for older versions, so building through it pins the floor where we want it.
#
# Only release builds are cross-linked: development builds go through plain
# cargo, which is faster and runs fine on the build machine.
export GLIBC_TARGET="${GLIBC_TARGET:-2.28}"
wants_release=0
for arg in "$@"; do
    [[ "$arg" == "--release" ]] && wants_release=1
done

# Vendored if it is there, otherwise whatever is installed, otherwise fetched -
# in that order. ffmpeg and nasm are built from source on first use and zig was
# not, which meant a machine where nobody had unpacked it by hand quietly built
# against its own glibc and shipped an archive that only ran on machines like
# it. Only fetched for a release build: a development build does not need it.
ZIG="$HERE/vendor/toolchain/zig/zig"
[[ -x "$ZIG" ]] || ZIG="$(command -v zig || echo "$ZIG")"
if [[ "$wants_release" == "1" && ! -x "$ZIG" ]]; then
    "$HERE/vendor/get-zig.sh"
    ZIG="$HERE/vendor/toolchain/zig/zig"
fi
export ZIG_GLOBAL_CACHE_DIR="$HERE/vendor/toolchain/zig-cache"
export ZIG_LOCAL_CACHE_DIR="$ZIG_GLOBAL_CACHE_DIR"

use_zig=0
if [[ "$wants_release" == "1" ]] && [[ -x "$ZIG" ]] \
   && command -v cargo-zigbuild >/dev/null; then
    use_zig=1
fi

cd "$HERE"
if [[ "$use_zig" == "1" ]]; then
    export PATH="$(dirname "$ZIG"):$PATH"
    target="x86_64-unknown-linux-gnu.$GLIBC_TARGET"
    set -- "${@/#build/zigbuild}"
    echo "==> linking against glibc $GLIBC_TARGET" >&2
    exec cargo "$@" --target "$target"
fi
exec cargo "$@"
