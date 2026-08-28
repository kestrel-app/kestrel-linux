#!/usr/bin/env bash
# Keep the expensive derived directories outside the build directory.
#
#   ./ci-cache.sh        links target/ and vendor/{prefix,toolchain} to a cache
#
# On a shell runner the build directory is not ours. GitLab cleans it between
# jobs, and what it removes depends on GIT_CLEAN_FLAGS, on the git strategy,
# and on which concurrency slot the job landed in - none of which this
# repository controls. Leaving 4.6 GB of compiled crates, a built ffmpeg and a
# downloaded toolchain in there means discovering the answer the slow way, once
# per job.
#
# So they live in $HOME instead and are linked back in. git clean removes the
# links, which cost nothing to recreate; the directories they point at are
# somewhere git has no opinion about. That holds whatever the clean flags say,
# and across concurrency slots, which GIT_CLEAN_FLAGS alone does not.
#
# Everything here is derived and safe to lose - it is a cache, not state.
# Delete $KESTREL_CACHE to force a cold build.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
CACHE="${KESTREL_CACHE:-$HOME/.cache/kestrel-linux}"

link() {
    local name="$1" path="$HERE/$2"
    local store="$CACHE/$name"

    mkdir -p "$store" "$(dirname "$path")"

    if [ -L "$path" ]; then
        rm -f "$path"                      # ours from a previous job
    elif [ -d "$path" ]; then
        # A real directory: either the first run after this was introduced, or
        # a clean that spared it. Adopt its contents if the cache is empty,
        # otherwise the cache is the newer of the two and this is a leftover.
        if [ -z "$(ls -A "$store" 2>/dev/null)" ]; then
            echo "    adopting existing $2 into the cache"
            rmdir "$store"
            mv "$path" "$store"
        else
            rm -rf "$path"
        fi
    fi

    ln -sfn "$store" "$path"
    echo "    $2 -> $store"
}

echo "==> cache in $CACHE"
link target           target
link vendor-prefix    vendor/prefix
link vendor-toolchain vendor/toolchain
