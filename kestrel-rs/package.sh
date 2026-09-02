#!/usr/bin/env bash
# Assemble a release: one binary plus the ffmpeg libraries it links against.
#
#   ./package.sh            -> dist/kestrel-<version>-x86_64/  and a .tar.gz
#
# The layout is deliberately two components:
#
#   kestrel        the executable
#   lib/           libav*/libsw* shared objects
#
# The binary carries an rpath of $ORIGIN/lib, so it finds them wherever the
# directory is unpacked — no installation, no LD_LIBRARY_PATH, no root.
#
# Shipping ffmpeg as *shared* libraries is also what satisfies its LGPL terms:
# a recipient can replace them without needing anything else from us, so there
# is no obligation to publish object files for relinking.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
cd "$HERE"

VERSION="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
ARCH="$(uname -m)"
NAME="kestrel-${VERSION}-${ARCH}"
OUT="$HERE/dist/$NAME"

# Building is skippable so a caller that has already built - the release
# pipeline runs package.sh and appimage.sh back to back - does not pay for it
# twice. On its own each script still builds, so neither needs a wrapper to be
# useful by hand.
if [[ -n "${KESTREL_SKIP_BUILD:-}" ]]; then
    echo "==> KESTREL_SKIP_BUILD set, using the existing release build"
else
    echo "==> building release"
    "$HERE/build.sh" build --release >/dev/null
fi

rm -rf "$OUT"
mkdir -p "$OUT/lib"

# Release builds are cross-linked against an older glibc so the archive runs on
# distributions older than this one; that lands in a target-specific directory.
BIN="target/x86_64-unknown-linux-gnu/release/kestrel"
[[ -f "$BIN" ]] || BIN="target/release/kestrel"
cp "$BIN" "$OUT/kestrel"
# Copy the real objects and their sonames; ffmpeg installs a chain of symlinks
# and a tarball should carry both so the loader resolves the same way.
cp -P vendor/prefix/lib/*.so* "$OUT/lib/"

cat > "$OUT/THIRD-PARTY-NOTICES.md" <<'NOTICE'
# Third-party notices

## FFmpeg

This program uses libraries from the FFmpeg project (https://ffmpeg.org) under
the GNU Lesser General Public License version 2.1 or later (LGPL v2.1+). The
libraries are the files in `lib/` and are dynamically linked, so you may replace
them with your own build.

FFmpeg is used here **unmodified**, built from the official 7.1 release with:

    --disable-gpl --disable-nonfree --disable-version3 --enable-shared
    --disable-everything with only H.264/H.265 decoding, the RTSP/MP4 demuxers,
    MP4 muxing and swscale enabled

No GPL or non-free components are enabled. The exact configure line is in
`vendor/build-ffmpeg.sh` in the source repository, and the upstream source is at
https://ffmpeg.org/releases/ffmpeg-7.1.tar.xz

A copy of the LGPL is included as `LICENSE.ffmpeg`.

## Rust crates

The remainder of this program links Rust crates published under the MIT and
Apache-2.0 licences, including eframe/egui, ureq, serde, chrono and keyring.
NOTICE

# Ship the licence text itself rather than pointing at a URL.
if [[ -f vendor/build/ffmpeg-7.1/COPYING.LGPLv2.1 ]]; then
    cp vendor/build/ffmpeg-7.1/COPYING.LGPLv2.1 "$OUT/LICENSE.ffmpeg"
fi

cat > "$OUT/README.txt" <<'READ'
Kestrel — live camera wall and PTZ control for your NVR

Run it directly:

    ./kestrel

Nothing needs installing. The `lib/` directory next to the binary holds the
ffmpeg libraries it uses; keep the two together.

Optional, to add it to your application menu:

    ./install.sh

Configuration lives in ~/.config/kestrel/config.json (mode 0600). Passwords go
to the system keyring when one is available.

Check which build this is:

    ./kestrel --version
READ

# Ship the icons. The launcher below refers to them by name, and without the
# files installed alongside it that name resolves to whatever happens to be left
# in the icon theme — which is how a stale icon survives a rebuild.
for size in 16 24 32 48 64 128 256 512; do
    icon="assets/icon-${size}.png"
    [[ -f "$icon" ]] || continue
    dir="$OUT/icons/hicolor/${size}x${size}/apps"
    mkdir -p "$dir"
    cp "$icon" "$dir/kestrel.png"
done

cat > "$OUT/install.sh" <<'INSTALL'
#!/usr/bin/env bash
# Optional: register a launcher for the current user. No root required.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
APPS="$HOME/.local/share/applications"
ICONS="$HOME/.local/share/icons/hicolor"
mkdir -p "$APPS"

# Icons first: the desktop entry names one, so it has to exist before the entry
# is read. Copying over any previous version is deliberate — an older install
# leaves files here that nothing else would ever replace.
if [[ -d "$HERE/icons/hicolor" ]]; then
    for src in "$HERE"/icons/hicolor/*/apps/kestrel.png; do
        [[ -e "$src" ]] || continue
        rel="${src#"$HERE/icons/hicolor/"}"
        mkdir -p "$ICONS/$(dirname "$rel")"
        cp -f "$src" "$ICONS/$rel"
    done
    command -v gtk-update-icon-cache >/dev/null && \
        gtk-update-icon-cache -f -t "$ICONS" >/dev/null 2>&1 || true
fi
cat > "$APPS/kestrel.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Kestrel
GenericName=Camera Client
Comment=Live camera wall and PTZ control for your NVR
Exec=$HERE/kestrel
Icon=kestrel
Categories=AudioVideo;Video;Player;
Terminal=false
StartupNotify=true
StartupWMClass=kestrel
DESKTOP
command -v update-desktop-database >/dev/null && update-desktop-database "$APPS" || true
echo "Installed a launcher pointing at $HERE/kestrel"
echo "If the old icon lingers, log out and back in — desktops cache them aggressively."
INSTALL
chmod +x "$OUT/install.sh"

# Guard the portability floor. This is the failure that gets reported as "it
# does not launch": the loader refuses the binary outright with
#   version `GLIBC_2.39' not found
# and nothing in a normal build makes that visible on the machine that built it.
GLIBC_MAX="${GLIBC_TARGET:-2.28}"
echo "==> checking nothing requires a glibc newer than $GLIBC_MAX"
worst=""
for f in "$OUT/kestrel" "$OUT"/lib/*.so.*; do
    [[ -f "$f" && ! -L "$f" ]] || continue
    v="$(readelf -VW "$f" 2>/dev/null | sed -n '/Version needs/,/^$/p' \
         | grep -oE 'GLIBC_[0-9.]+' | sed 's/GLIBC_//' | sort -V | tail -1)"
    [[ -z "$v" ]] && continue
    if [[ "$(printf '%s\n%s\n' "$GLIBC_MAX" "$v" | sort -V | tail -1)" != "$GLIBC_MAX" ]]; then
        echo "    $(basename "$f") requires glibc $v" >&2
        worst="$v"
    fi
done
if [[ -n "$worst" ]]; then
    echo "ERROR: the bundle needs glibc $worst and will not start on older systems." >&2
    echo "       Build with ./build.sh build --release so it links through zig." >&2
    exit 1
fi
echo "    ok, floor is glibc $GLIBC_MAX or lower"

echo "==> verifying the bundle resolves its own libraries"
missing="$(cd "$OUT" && ldd ./kestrel | grep -c 'not found' || true)"
if [[ "$missing" != "0" ]]; then
    echo "ERROR: $missing library/libraries unresolved" >&2
    (cd "$OUT" && ldd ./kestrel | grep 'not found') >&2
    exit 1
fi
# Resolve the real test: unpack somewhere unrelated and confirm the loader
# picks the bundled libraries, not anything left over on the build machine.
probe="$(mktemp -d)"
cp -r "$OUT" "$probe/bundle"
bundled="$(cd "$probe/bundle" && ldd ./kestrel | grep -c "$probe/bundle" || true)"
outside="$(cd "$probe/bundle" && ldd ./kestrel | grep -cE "vendor/prefix|not found" || true)"
rm -rf "$probe"
if [[ "$bundled" -lt 5 || "$outside" != "0" ]]; then
    echo "ERROR: relocated bundle does not use its own libraries" >&2
    exit 1
fi
echo "    $bundled ffmpeg libraries resolved from the bundle after relocation"

tar -C "$HERE/dist" -czf "$HERE/dist/$NAME.tar.gz" "$NAME"

echo
echo "==> $OUT"
du -sh "$OUT" "$HERE/dist/$NAME.tar.gz" | sed 's/^/    /'
find "$OUT" -maxdepth 1 -mindepth 1 -printf '    %f\n' | sort
