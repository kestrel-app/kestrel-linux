#!/usr/bin/env bash
# Build a single-file AppImage.
#
#   ./appimage.sh   ->  dist/Kestrel-<version>-x86_64.AppImage
#
# This is the same two components as the tarball — the binary plus ffmpeg's
# shared libraries — wrapped so a user downloads one file. ffmpeg stays a set of
# *shared* objects inside the image, which is what keeps the LGPL obligation
# satisfied without publishing object files for relinking.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
cd "$HERE"

VERSION="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
ARCH="$(uname -m)"
APPDIR="$HERE/dist/AppDir"
OUTPUT="$HERE/dist/Kestrel-${VERSION}-${ARCH}.AppImage"
TOOLS="$HERE/vendor/tools"

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

# ---------------------------------------------------------------- AppDir
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/lib" \
         "$APPDIR/usr/share/applications" \
         "$APPDIR/usr/share/icons/hicolor/256x256/apps"

# Release builds are cross-linked against an older glibc so the archive runs on
# distributions older than this one; that lands in a target-specific directory.
BIN="target/x86_64-unknown-linux-gnu/release/kestrel"
[[ -f "$BIN" ]] || BIN="target/release/kestrel"
cp "$BIN" "$APPDIR/usr/bin/kestrel"
# -P keeps ffmpeg's soname symlink chain intact so the loader resolves the same
# way it does in the build tree.
cp -P vendor/prefix/lib/*.so* "$APPDIR/usr/lib/"

for size in 16 24 32 48 64 128 256 512; do
    icon="assets/icon-${size}.png"
    [[ -f "$icon" ]] || continue
    dir="$APPDIR/usr/share/icons/hicolor/${size}x${size}/apps"
    mkdir -p "$dir"
    cp "$icon" "$dir/kestrel.png"
done
# appimagetool expects the icon and .desktop at the AppDir root as well.
cp assets/icon-256.png "$APPDIR/kestrel.png"

cat > "$APPDIR/usr/share/applications/kestrel.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=Kestrel
GenericName=Camera Client
Comment=Live camera wall and PTZ control for your NVR
Exec=kestrel
Icon=kestrel
Categories=AudioVideo;Video;Player;
Keywords=camera;cctv;nvr;reolink;surveillance;rtsp;
Terminal=false
StartupNotify=true
StartupWMClass=kestrel
DESKTOP
cp "$APPDIR/usr/share/applications/kestrel.desktop" "$APPDIR/kestrel.desktop"

cat > "$APPDIR/AppRun" <<'APPRUN'
#!/usr/bin/env bash
# The binary already carries an rpath of $ORIGIN/../lib, so it finds the bundled
# ffmpeg without LD_LIBRARY_PATH — which matters, because forcing that variable
# would also override the host's own libraries for any child process.
HERE="$(dirname "$(readlink -f "${0}")")"
export PATH="$HERE/usr/bin:$PATH"
exec "$HERE/usr/bin/kestrel" "$@"
APPRUN
chmod +x "$APPDIR/AppRun"

# ---------------------------------------------------------------- appimagetool
mkdir -p "$TOOLS"
if [[ ! -x "$TOOLS/appimagetool/AppRun" ]]; then
    echo "==> fetching appimagetool"
    curl -sSLo "$TOOLS/appimagetool.AppImage" \
        "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-${ARCH}.AppImage"
    chmod +x "$TOOLS/appimagetool.AppImage"
    # Self-mounting needs libfuse2, which this machine does not have. Unpacking
    # the tool sidesteps that entirely.
    (cd "$TOOLS" && ./appimagetool.AppImage --appimage-extract >/dev/null)
    mv "$TOOLS/squashfs-root" "$TOOLS/appimagetool"
fi

echo "==> packing"
rm -f "$OUTPUT"
ARCH="$ARCH" "$TOOLS/appimagetool/AppRun" --no-appstream "$APPDIR" "$OUTPUT" 2>&1 |
    grep -viE "^$|appstream|Generating|Embedding|Marking|Using architecture" || true

[[ -f "$OUTPUT" ]] || { echo "ERROR: appimagetool produced nothing" >&2; exit 1; }
chmod +x "$OUTPUT"

# ---------------------------------------------------------------- verify
echo "==> verifying the image"
probe="$(mktemp -d)"
(cd "$probe" && "$OUTPUT" --appimage-extract >/dev/null 2>&1) || {
    echo "ERROR: could not extract the produced AppImage" >&2
    rm -rf "$probe"; exit 1
}
root="$probe/squashfs-root"
missing="$(ldd "$root/usr/bin/kestrel" 2>/dev/null | grep -c 'not found' || true)"
inside="$(ldd "$root/usr/bin/kestrel" 2>/dev/null | grep -c "$root" || true)"
outside="$(ldd "$root/usr/bin/kestrel" 2>/dev/null | grep -c 'vendor/prefix' || true)"
rm -rf "$probe"

if [[ "$missing" != "0" || "$outside" != "0" || "$inside" -lt 5 ]]; then
    echo "ERROR: the image does not use its own ffmpeg (inside=$inside outside=$outside missing=$missing)" >&2
    exit 1
fi
echo "    $inside ffmpeg libraries resolved from inside the image"

echo
echo "==> $OUTPUT"
du -h "$OUTPUT" | sed 's/^/    /'
echo
echo "    Run it directly:            $(basename "$OUTPUT")"
echo "    Without libfuse2 installed: $(basename "$OUTPUT") --appimage-extract-and-run"
