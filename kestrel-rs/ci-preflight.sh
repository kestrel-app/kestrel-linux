#!/usr/bin/env bash
# What the build needs from the machine it runs on.
#
# build.sh vendors ffmpeg and, for release builds, zig - so most of the
# toolchain is self-contained. What it cannot supply is a C compiler, a Rust
# toolchain and libclang, because bindgen loads libclang at runtime to read
# ffmpeg's headers. Checking for them up front turns "error: linker `cc` not
# found" three minutes into a build into one line at the start of the job.
#
#   ./ci-preflight.sh
set -euo pipefail

missing=()

need() {
    command -v "$1" >/dev/null 2>&1 || missing+=("$2")
}

need cargo      "cargo/rustc  - a Rust toolchain (rustup, or the distro's cargo)"
need cc         "cc           - a C compiler (build-essential)"
need make       "make         - (build-essential)"
need pkg-config "pkg-config   - pkg-config"
need curl       "curl         - curl"
need tar        "tar          - tar"

# bindgen dlopen()s libclang rather than running the clang binary, so the
# binary being absent is fine and the shared library being absent is not.
# nullglob, and an array rather than `ls`: `ls a b` where only b exists still
# exits non-zero, so testing several candidate paths with one ls reports a
# library that is sitting right there as missing.
shopt -s nullglob
libclang=(
    /usr/lib/*/libclang*.so*
    /usr/lib/llvm-*/lib/libclang*.so*
    /usr/lib64/libclang*.so*
)
shopt -u nullglob
if ((${#libclang[@]} == 0)); then
    missing+=("libclang.so   - libclang-dev (bindgen reads ffmpeg's headers with it)")
fi

if ((${#missing[@]})); then
    echo "This machine cannot build Kestrel yet. Missing:" >&2
    printf '  %s\n' "${missing[@]}" >&2
    echo >&2
    echo "On Debian/Ubuntu:" >&2
    echo "  apt install build-essential pkg-config libclang-dev curl" >&2
    echo "  # and a Rust toolchain: https://rustup.rs" >&2
    exit 1
fi

# Not required: build-ffmpeg.sh compiles nasm from source when the machine has
# no assembler. Having one just saves a couple of minutes on the first build.
if ! command -v nasm >/dev/null && ! command -v yasm >/dev/null; then
    echo "note: no nasm/yasm; the first ffmpeg build will compile nasm itself" >&2
fi

# --release: what a *shippable* build additionally needs.
#
# build.sh only takes the zig cross-linking path when both the vendored zig and
# cargo-zigbuild are present, and falls back to plain cargo when either is
# missing. That fallback is the dangerous one: it succeeds, produces a binary
# that runs perfectly on the build machine, and silently records the build
# machine's glibc as a hard floor - so the archive refuses to start on anything
# older, which is the entire point of shipping one. Nothing about the artifact
# says so. Better to refuse to build it.
if [ "${1:-}" = "--release" ]; then
    HERE="$(cd "$(dirname "$0")" && pwd)"
    release_missing=()

    # zig is not listed: build.sh fetches it via vendor/get-zig.sh when a
    # release build needs one. cargo-zigbuild is the part nothing can fetch for
    # you, because it is a cargo subcommand rather than a file in vendor/.
    if [ ! -x "$HERE/vendor/toolchain/zig/zig" ] && ! command -v zig >/dev/null 2>&1; then
        echo "note: no zig yet; the release build will download one" >&2
    fi
    command -v cargo-zigbuild >/dev/null 2>&1 || release_missing+=(
        "cargo-zigbuild - cargo install cargo-zigbuild")

    if ((${#release_missing[@]})); then
        echo "Refusing to build a release without the zig cross-linker." >&2
        printf '  %s\n' "${release_missing[@]}" >&2
        echo >&2
        echo "Without it the build still succeeds, but pins the glibc floor to" >&2
        echo "this machine's - so the archive will not start on an older" >&2
        echo "distribution, and nothing about the file says why." >&2
        exit 1
    fi
    # `|| true` is load-bearing: under `set -e` a failing command substitution
    # in an assignment ends the script, so without it this exits 1 in exactly
    # the case the note above has just said is fine.
    zig="$HERE/vendor/toolchain/zig/zig"
    [ -x "$zig" ] || zig="$(command -v zig || true)"

    if [ -n "$zig" ] && [ -x "$zig" ]; then
        found="zig $("$zig" version 2>/dev/null) ($zig)"
    else
        found="zig to be downloaded"
    fi
    echo "release preflight ok: $(cargo-zigbuild --version 2>&1 | head -1), $found"
fi

echo "preflight ok: $(cargo --version), $(cc --version | head -1)"
