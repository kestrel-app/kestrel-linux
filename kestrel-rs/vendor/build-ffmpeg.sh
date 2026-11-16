#!/usr/bin/env bash
# Build the ffmpeg that Kestrel links against.
#
# Deliberately configured for a *distributable* client:
#
#   * LGPL only — no --enable-gpl, no --enable-nonfree. Kestrel decodes; the
#     GPL pieces (libx264/libx265) are encoders we never call. This keeps the
#     obligation to "let the user replace the library", which shipping the .so
#     files satisfies outright.
#   * Shared libraries, so the LGPL relinking requirement is met by the shared
#     library mechanism and no object files need to be published.
#   * --disable-everything plus an explicit component list: only the codecs,
#     demuxers and protocols this app actually uses. Smaller download, smaller
#     attack surface, faster build.
#
#     avfilter/avdevice are built even though Kestrel calls neither:
#     ffmpeg-sys-next probes for all of them unconditionally. With
#     --disable-everything they compile to near-empty stubs, so the cost is a
#     couple of hundred KB rather than real bloat. swresample *is* used, to turn
#     the cameras' 16 kHz mono AAC into whatever rate the sound card wants.
#   * x86 assembly enabled. nasm is built from source first if the system has
#     no assembler, because losing ffmpeg's hand-written SIMD roughly halves
#     decode throughput — which matters when 16 cameras decode at once.
#
# Everything installs under vendor/prefix; nothing touches the system.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
PREFIX="$HERE/prefix"
BUILD="$HERE/build"
JOBS="$(nproc)"

FFMPEG_VERSION="${FFMPEG_VERSION:-7.1}"

# Oldest glibc the shipped binaries must run against.
#
# Linking on a modern distro bakes that distro's glibc in as a hard floor: a
# build on Debian 13 demanded GLIBC_2.39 and refused to start on anything older,
# which defeats the point of shipping a copyable archive. Zig carries glibc stubs
# for every version, so compiling through it pins the floor wherever we choose
# regardless of what the build machine runs.
GLIBC_TARGET="${GLIBC_TARGET:-2.28}"
ZIG="$HERE/toolchain/zig/zig"
[[ -x "$ZIG" ]] || ZIG="$(command -v zig || echo "$ZIG")"
NASM_VERSION="${NASM_VERSION:-2.16.03}"

mkdir -p "$BUILD" "$PREFIX"
export PATH="$PREFIX/bin:$PATH"

# ---------------------------------------------------------------- nasm
if ! command -v nasm >/dev/null && ! command -v yasm >/dev/null; then
    echo "==> no assembler found; building nasm $NASM_VERSION"
    cd "$BUILD"
    if [[ ! -d "nasm-$NASM_VERSION" ]]; then
        curl -sSLo "nasm-$NASM_VERSION.tar.xz" \
            "https://www.nasm.us/pub/nasm/releasebuilds/$NASM_VERSION/nasm-$NASM_VERSION.tar.xz"
        tar xf "nasm-$NASM_VERSION.tar.xz"
    fi
    cd "nasm-$NASM_VERSION"
    ./configure --prefix="$PREFIX" >/dev/null
    make -j"$JOBS" >/dev/null
    make install >/dev/null
    echo "==> nasm $(nasm -v | head -1)"
fi

# ---------------------------------------------------------------- ffmpeg
cd "$BUILD"
if [[ ! -d "ffmpeg-$FFMPEG_VERSION" ]]; then
    echo "==> fetching ffmpeg $FFMPEG_VERSION"
    curl -sSLo "ffmpeg-$FFMPEG_VERSION.tar.xz" \
        "https://ffmpeg.org/releases/ffmpeg-$FFMPEG_VERSION.tar.xz"
    tar xf "ffmpeg-$FFMPEG_VERSION.tar.xz"
fi
cd "ffmpeg-$FFMPEG_VERSION"

# Audio: Reolink publishes AAC (16 kHz mono, on both main and sub streams);
# G.711 covers the older models that send pcm_alaw/pcm_mulaw instead.
# aac_adtstoasc is what lets that audio be copied into an MP4 recording.
CC_ARGS=()
if [[ -x "$ZIG" ]]; then
    # zig needs a writable cache; keep it beside the toolchain rather than in
    # $HOME so a clean checkout behaves the same as a repeat build.
    export ZIG_GLOBAL_CACHE_DIR="$HERE/toolchain/zig-cache"
    export ZIG_LOCAL_CACHE_DIR="$HERE/toolchain/zig-cache"
    mkdir -p "$ZIG_GLOBAL_CACHE_DIR"
    CC_ARGS=(
        --cc="$ZIG cc -target x86_64-linux-gnu.$GLIBC_TARGET"
        --ar="$ZIG ar"
        --ranlib="$ZIG ranlib"
    )
    echo "==> targeting glibc $GLIBC_TARGET via zig"
else
    # Not a warning any more. These libraries are linked into every build,
    # debug and release alike, and a release cannot link against an ffmpeg
    # built for a newer glibc than it targets - lld refuses with a page of
    # "undefined reference: pthread_create@GLIBC_2.34". Carrying on here means
    # the failure surfaces much later, in a different job, as somebody else's
    # problem.
    echo "==> no zig toolchain; fetching one" >&2
    "$HERE/get-zig.sh"
    ZIG="$HERE/toolchain/zig/zig"
    if [[ ! -x "$ZIG" ]]; then
        echo "ERROR: ffmpeg must be linked through zig, and none could be" >&2
        echo "       obtained. See vendor/get-zig.sh." >&2
        exit 1
    fi
    export ZIG_GLOBAL_CACHE_DIR="$HERE/toolchain/zig-cache"
    export ZIG_LOCAL_CACHE_DIR="$HERE/toolchain/zig-cache"
    mkdir -p "$ZIG_GLOBAL_CACHE_DIR"
    CC_ARGS=(
        --cc="$ZIG cc -target x86_64-linux-gnu.$GLIBC_TARGET"
        --ar="$ZIG ar"
        --ranlib="$ZIG ranlib"
    )
    echo "==> targeting glibc $GLIBC_TARGET via zig"
fi

CONFIGURE_ARGS=(
    --prefix="$PREFIX"
    "${CC_ARGS[@]}"
    --enable-shared --disable-static
    --enable-pic
    --disable-gpl --disable-nonfree --disable-version3
    --disable-programs --disable-doc --disable-postproc
    --disable-everything
    --enable-network
    --enable-decoder=h264,hevc,mjpeg,rawvideo,aac,aac_latm,pcm_alaw,pcm_mulaw,pcm_s16le,pcm_s16be
    --enable-parser=h264,hevc,mjpeg,aac,aac_latm
    --enable-demuxer=rtsp,sdp,mov,mp4,h264,hevc,mjpeg,image2,aac
    --enable-muxer=mp4,mov
    --enable-protocol=file,rtp,rtsp,tcp,udp,http,pipe
    --enable-bsf=h264_mp4toannexb,hevc_mp4toannexb,extract_extradata,aac_adtstoasc
    --enable-swscale
    --enable-swresample
)

CONFIG_STAMP="$BUILD/.configure-args"
# Hash the arguments themselves. The previous stamp hashed a fixed string, so
# changing the component list never triggered a reconfigure and the rebuild
# silently kept the old feature set.
ARGS_HASH="$(printf '%s\n' "${CONFIGURE_ARGS[@]}" | sha256sum | cut -d' ' -f1)"
if [[ ! -f config.h || ! -f "$CONFIG_STAMP" || "$(cat "$CONFIG_STAMP")" != "$ARGS_HASH" ]]; then
    echo "==> configuring ffmpeg (LGPL, shared, minimal)"
    ./configure "${CONFIGURE_ARGS[@]}" >/dev/null

    # ffmpeg probes for sysctl by linking the bare symbol without including its
    # header. Targeting an older glibc that still exports `sysctl` while using
    # modern headers that no longer declare <sys/sysctl.h> makes that probe say
    # yes and the compile then fail. Nothing on Linux needs it — it is the BSD
    # and macOS core-count path; Linux uses sched_getaffinity.
    if grep -q "^#define HAVE_SYSCTL 1" config.h; then
        sed -i 's/^#define HAVE_SYSCTL 1$/#define HAVE_SYSCTL 0/' config.h
        sed -i 's/^HAVE_SYSCTL=yes$/HAVE_SYSCTL=no/' ffbuild/config.mak
        echo "==> disabled the sysctl path (header absent in the target's headers)"
    fi

    echo "$ARGS_HASH" > "$CONFIG_STAMP"
fi

echo "==> building ffmpeg with $JOBS jobs"
make -j"$JOBS" >/dev/null
make install >/dev/null

echo
echo "==> installed to $PREFIX"
ls -1 "$PREFIX/lib"/*.so.* 2>/dev/null | head
du -sh "$PREFIX/lib"
echo "==> license check (must say LGPL, and non-free must be no):"
grep -E "^(CONFIG_GPL|CONFIG_NONFREE)=" config.h || echo "  GPL/nonfree not enabled"

# What this prefix was built for. build.sh compares it against GLIBC_TARGET and
# rebuilds when they differ, so a prefix built by an older arrangement - or for
# a different floor - is replaced rather than silently linked against.
echo "$GLIBC_TARGET" > "$PREFIX/.built-for-glibc"
