#!/usr/bin/env python3
"""Attach built artifacts to a GitHub release.

    GITHUB_REPO=owner/name GITHUB_TOKEN=... tools/github-release.py v0.1.0 dist/*.tar.gz

Creates the release if it does not exist and replaces any asset of the same
name, so re-running it after a rebuild does the obvious thing rather than
failing on a duplicate.

Written against urllib rather than curl for one reason: the token stays in the
environment. A curl command line carries it in argv, where `ps` on a shared
runner will show it to anybody with an account on the machine.

Needs a token with contents:write. That is the same permission the publish job
already requires, so no second credential.
"""

from __future__ import annotations

import json
import os
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

API = "https://api.github.com"
UPLOADS = "https://uploads.github.com"

# The runner's link to GitHub drops the odd connection. A reset, a timeout or a
# 5xx is tried again; a 4xx is an answer, and is not.
ATTEMPTS = 5
TIMEOUT = 300  # seconds; an upload is tens of MB


def transient(error: Exception) -> bool:
    if isinstance(error, urllib.error.HTTPError):
        return error.code >= 500
    return isinstance(error, (urllib.error.URLError, OSError))


def retrying(what: str, action):
    """Run `action` until it works, a few times over transient failures."""
    for attempt in range(1, ATTEMPTS + 1):
        try:
            return action()
        except Exception as error:
            if attempt == ATTEMPTS or not transient(error):
                raise
            wait = 5 * attempt
            print(f"  {what}: {error}; trying again in {wait}s", file=sys.stderr)
            time.sleep(wait)


def call(url: str, token: str, method="GET", data=None, content_type=None):
    request = urllib.request.Request(url, data=data, method=method)
    request.add_header("Authorization", f"Bearer {token}")
    request.add_header("Accept", "application/vnd.github+json")
    request.add_header("X-GitHub-Api-Version", "2022-11-28")
    request.add_header("User-Agent", "kestrel-release")
    if content_type:
        request.add_header("Content-Type", content_type)
    with urllib.request.urlopen(request, timeout=TIMEOUT) as response:
        body = response.read()
    return json.loads(body) if body else {}


def release_for(repo: str, tag: str, token: str) -> dict:
    """The release for this tag, created if it is not there yet."""
    try:
        return retrying("reading the release",
                        lambda: call(f"{API}/repos/{repo}/releases/tags/{tag}", token))
    except urllib.error.HTTPError as error:
        if error.code != 404:
            raise

    payload = json.dumps(
        {
            "tag_name": tag,
            "name": tag,
            # Whatever the published branch points at now. The publish job
            # force-pushes a fresh parentless commit each time, so there is no
            # stable earlier commit to hang this on.
            "target_commitish": os.environ.get("GITHUB_BRANCH", "main"),
            "body": "Built and published from GitLab CI.",
        }
    ).encode()
    print(f"creating release {tag}")
    # Not retried: a create whose reply was lost may have happened, and a
    # second would fail as a duplicate. Re-running the job finds it instead.
    return call(f"{API}/repos/{repo}/releases", token, "POST", payload,
                "application/json")


def upload(repo: str, release: dict, path: Path, token: str) -> None:
    name = path.name
    blob = path.read_bytes()

    def attempt():
        # The assets are read afresh each time: an upload cut off part way
        # leaves a broken asset of this name behind, which has to go first.
        assets = call(f"{API}/repos/{repo}/releases/{release['id']}/assets?per_page=100", token)
        for asset in assets:
            if asset["name"] == name:
                print(f"  replacing existing {name}")
                call(f"{API}/repos/{repo}/releases/assets/{asset['id']}", token,
                     "DELETE")
        print(f"  uploading {name} ({len(blob) / 1e6:.1f} MB)")
        call(
            f"{UPLOADS}/repos/{repo}/releases/{release['id']}/assets?name={name}",
            token, "POST", blob, "application/octet-stream",
        )

    retrying(f"uploading {name}", attempt)


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        print(__doc__, file=sys.stderr)
        return 2

    repo = os.environ.get("GITHUB_REPO", "")
    token = os.environ.get("GITHUB_TOKEN", "")
    if not repo or not token:
        print("GITHUB_REPO and GITHUB_TOKEN must both be set", file=sys.stderr)
        return 2

    tag, names = argv[0], argv[1:]
    files = [Path(n) for n in names]
    for path in files:
        if not path.is_file():
            print(f"no such file: {path}", file=sys.stderr)
            return 1

    release = release_for(repo, tag, token)
    for path in files:
        upload(repo, release, path, token)

    print(f"\n{release['html_url']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
