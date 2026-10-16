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
import urllib.error
import urllib.request
from pathlib import Path

API = "https://api.github.com"
UPLOADS = "https://uploads.github.com"


def call(url: str, token: str, method="GET", data=None, content_type=None):
    request = urllib.request.Request(url, data=data, method=method)
    request.add_header("Authorization", f"Bearer {token}")
    request.add_header("Accept", "application/vnd.github+json")
    request.add_header("X-GitHub-Api-Version", "2022-11-28")
    request.add_header("User-Agent", "kestrel-release")
    if content_type:
        request.add_header("Content-Type", content_type)
    with urllib.request.urlopen(request) as response:
        body = response.read()
    return json.loads(body) if body else {}


def release_for(repo: str, tag: str, token: str) -> dict:
    """The release for this tag, created if it is not there yet."""
    try:
        return call(f"{API}/repos/{repo}/releases/tags/{tag}", token)
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
    return call(f"{API}/repos/{repo}/releases", token, "POST", payload,
                "application/json")


def upload(repo: str, release: dict, path: Path, token: str) -> None:
    name = path.name

    for asset in release.get("assets", []):
        if asset["name"] == name:
            print(f"  replacing existing {name}")
            call(f"{API}/repos/{repo}/releases/assets/{asset['id']}", token,
                 "DELETE")

    blob = path.read_bytes()
    print(f"  uploading {name} ({len(blob) / 1e6:.1f} MB)")
    call(
        f"{UPLOADS}/repos/{repo}/releases/{release['id']}/assets?name={name}",
        token, "POST", blob, "application/octet-stream",
    )


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
