#!/usr/bin/env python3
"""Refuse to publish things that should not be public.

A push to a public remote is irreversible in the way that matters: the commit
is fetched, mirrored and indexed before anybody notices it was wrong. Deleting
it afterwards deletes nothing. So the check has to run before the push, which
is what the pre-push hook is for.

This is the shared version, installed once for every repository via

    git config --global core.hooksPath ~/.config/git/hooks

so a repository created next month is covered without anybody remembering to
install anything. A repository that wants its own rules keeps them in its own
tools/check-public.py, and the shared hook runs that instead.

Two things get looked at:

  * every blob reachable from the commits being pushed - not just the working
    tree, because a public repository publishes its history too, and a
    hostname deleted last month is still in last month's commit;
  * the author and committer of those commits, because an internal mail
    domain in the metadata leaks exactly as much as one in a file.

Run by hand to see where a repository stands:

    python3 ~/.config/git/check-public.py                    # working tree
    python3 ~/.config/git/check-public.py --history          # every commit, ever
    python3 ~/.config/git/check-public.py --range main..HEAD # what a change adds

(--push is how the hook invokes it, and reads the refs git puts on stdin.)

--range is what CI uses on a merge request. It runs before the change lands on
the branch that gets mirrored, which is the last point at which a pipeline can
still prevent something rather than just report it: the mirror fires when a
push lands, not when a pipeline passes.

Deliberately narrow. A scanner that reports something on every run is a
scanner somebody turns off, and then it is not a control at all. To settle a
line it is wrong about, end the line with '# noqa: public'; to settle a whole
class of them, see .check-public.json below.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
from pathlib import Path, PurePosixPath

# Per-repository settings, read from .check-public.json at the top level. Every
# key is optional; a repository with nothing to say needs no file at all.
#
#   {
#     "internal_names":  ["uex\\.internal", "site0-[a-z0-9]+"],
#     "allow_emails":    ["support@example\\.com"],
#     "allow_patterns":  ["regex for something this repo is right about"],
#     "fixture_dirs":    ["tests/", "dev/"],
#     "allow_unwanted":  ["docs/CLAUDE.md"]
#   }
DEFAULTS = {
    # The network this work was written on. These are the names that say where
    # the machines are, and they have no business in a public repository.
    "internal_names": [r"(?:[a-z0-9-]+\.)?uex\.internal", r"site0-[a-z0-9-]+"],
    # Addresses that are meant to be read by strangers, so not a leak.
    "allow_emails": [
        r"noreply@anthropic\.com",
        r"[^@]+@(?:users\.)?noreply\.github\.com",
        # OData annotations - 'Members@odata.count' is a JSON key, not a mailbox.
        r"[A-Za-z0-9._%+-]+@odata\.[A-Za-z]+",
    ],
    "allow_patterns": [],
    # Where an address may look real. A fixture needs something that parses,
    # and a private address in a test says nothing about anybody's network;
    # prose gets the RFC 5737 range instead, which cannot be mistaken for a
    # machine somebody owns.
    "fixture_dirs": ["tests/", "test/", "dev/"],
    "allow_unwanted": [],
}

ZERO = "0" * 40


def load_config(root: Path) -> dict:
    config = dict(DEFAULTS)
    path = root / ".check-public.json"
    if path.exists():
        try:
            config.update(json.loads(path.read_text(encoding="utf-8")))
        except (OSError, ValueError) as exc:
            print(f"warning: ignoring {path}: {exc}", file=sys.stderr)
    return config


def build_checks(config: dict) -> list[tuple[str, re.Pattern[str], str]]:
    internal = "|".join(f"(?:{name})" for name in config["internal_names"])
    allowed_mail = "|".join(f"(?:{mail})" for mail in config["allow_emails"]) or r"(?!)"

    return [
        (
            "internal hostname",
            re.compile(rf"\b(?:{internal})\b", re.I),
            "names the network this was written on; use example.com",
        ),
        (
            "private address",
            # 172.30.32.0/23 is the Home Assistant supervisor network. It is
            # the same on every installation and documented as such, so it
            # says nothing about whose machine this is.
            re.compile(
                r"\b(?!172\.30\.3[23]\.)"
                r"(?:10\.\d{1,3}|192\.168|172\.(?:1[6-9]|2\d|3[01]))"
                r"\.\d{1,3}\.\d{1,3}\b"
            ),
            "a real-looking address outside tests; use the RFC 5737 range "
            "(192.0.2.x, 198.51.100.x, 203.0.113.x)",
        ),
        (
            "private key",
            re.compile(r"-----BEGIN [A-Z ]*PRIVATE KEY-----"),
            "a private key must never be committed, published or not",
        ),
        (
            "access token",
            re.compile(
                r"\b(?:gh[pousr]_[A-Za-z0-9]{16,}|github_pat_[A-Za-z0-9_]{20,}"
                r"|glpat-[A-Za-z0-9_-]{20,}|xox[baprs]-[A-Za-z0-9-]{10,}"
                r"|AKIA[0-9A-Z]{16}|sk-[A-Za-z0-9]{32,})\b"
            ),
            "looks like a real credential; revoke it before anything else",
        ),
        (
            "personal address",
            # The lookbehind keeps this off the credentials in a URL: the "pw@"
            # of rtsp://admin:pw@nvr.local is not somebody's mailbox, and a
            # check that says it is gets ignored along with what it gets right.
            re.compile(
                rf"(?<![:/\w.%+-])(?!{allowed_mail})"
                r"[A-Za-z0-9._%+-]+@"
                r"(?!example\.|[A-Za-z0-9.-]*\.example\b|noreply\.)"
                # pi@weather.local and git@host.internal are somewhere to
                # connect, not somewhere to write to. The internal name in the
                # second one is the hostname check's business, and reporting it
                # twice under the wrong heading only teaches you to skim.
                r"(?![A-Za-z0-9.-]+\.(?:local|internal|arpa)\b)"
                # A lowercase TLD. 'ResetType@Redfish.AllowableValues' is a
                # Redfish property name and has no business being called mail.
                r"[A-Za-z0-9.-]+\.[a-z]{2,}\b"
            ),
            "a personal mailbox in a public repository collects spam for ever",
        ),
    ]


# Files that are about how the work was done rather than what was built, and
# have no business in something the public reads.
UNWANTED = re.compile(
    r"(^|/)(CLAUDE\.md|AGENTS\.md|\.claude/|\.cursor/|\.aider|"
    r"\.env(\.|$)|.*\.local\.(json|ya?ml))",
    re.I,
)

# Lines that exist to describe the thing being looked for, and would otherwise
# report themselves for ever.
EXEMPT = re.compile(r"check-public|# noqa: public")


def git(root: Path, *args: str) -> str:
    out = subprocess.run(
        ["git", *args], cwd=root, capture_output=True, text=True, check=True
    )
    return out.stdout


def in_fixture_dir(name: str, fixture_dirs) -> bool:
    """Whether any directory on the way to this file is a fixture directory.

    Matching only the front of the path misses addon/foo/tests/test_x.py, which
    is where most of the fixtures in these repositories actually live.
    """
    wanted = {d.strip("/") for d in fixture_dirs}
    return any(part in wanted for part in PurePosixPath(name).parts[:-1])


def scan_text(name: str, text: str, checks, fixture_dirs, allow, where="") -> list[str]:
    problems = []
    in_fixtures = in_fixture_dir(name, fixture_dirs)

    for number, line in enumerate(text.splitlines(), 1):
        if EXEMPT.search(line) or any(pattern.search(line) for pattern in allow):
            continue
        for label, pattern, why in checks:
            if label == "private address" and in_fixtures:
                continue
            found = pattern.search(line)
            if found:
                problems.append(
                    f"{where}{name}:{number}: {label} "
                    f"'{found.group(0)}' - {why}"
                )
    return problems


def unwanted(name: str, allow_unwanted) -> bool:
    return bool(UNWANTED.search(name)) and name not in allow_unwanted


def scan_worktree(root: Path, config, checks, allow) -> list[str]:
    problems: list[str] = []
    for name in git(root, "ls-files").splitlines():
        if not name:
            continue
        if unwanted(name, config["allow_unwanted"]):
            problems.append(f"{name}: should not be published at all")
            continue
        try:
            text = (root / name).read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue  # binary, or gone; nothing to read
        problems += scan_text(name, text, checks, config["fixture_dirs"], allow)
    return problems


def scan_commits(root: Path, revs, config, checks, allow, prefix="history ") -> list[str]:
    """Every blob and every author reachable from the commits being pushed.

    A public repository publishes its history, so the question is not what the
    working tree says today but what somebody can read out of any commit in it.
    """
    problems: list[str] = []

    # Authors and committers. An internal mail domain here is not visible in
    # any file, which is exactly why it survives a review of the files.
    for line in set(git(root, "log", "--format=%ae%n%ce", *revs).splitlines()):
        if not line:
            continue
        for label, pattern, why in checks:
            if label not in ("personal address", "internal hostname"):
                continue
            if pattern.search(line):
                problems.append(
                    f"commit metadata: {label} '{line}' - {why}; "
                    f"set user.email in this repository, and rewrite the "
                    f"commits that already carry it"
                )
                break

    # Every blob, once, whatever commit it came from.
    listing = git(root, "rev-list", "--objects", *revs).splitlines()
    blobs: dict[str, str] = {}
    for line in listing:
        sha, _, name = line.partition(" ")
        if name and not unwanted(name, config["allow_unwanted"]):
            blobs.setdefault(sha, name)
        elif name:
            problems.append(f"{prefix}{name}: should not be published at all")

    if not blobs:
        return problems

    proc = subprocess.run(
        ["git", "cat-file", "--batch"],
        cwd=root,
        input="\n".join(blobs).encode(),
        capture_output=True,
        check=True,
    )
    data, at = proc.stdout, 0
    while at < len(data):
        end = data.find(b"\n", at)
        if end < 0:
            break
        header = data[at:end].decode(errors="replace").split()
        if len(header) != 3:
            break  # a missing object; git said what it could
        sha, kind, size = header[0], header[1], int(header[2])
        body = data[end + 1 : end + 1 + size]
        at = end + 1 + size + 1
        if kind != "blob":
            continue
        try:
            text = body.decode("utf-8")
        except UnicodeDecodeError:
            continue  # binary; nothing to read
        problems += scan_text(
            blobs[sha], text, checks, config["fixture_dirs"], allow, where=prefix
        )

    return problems


def pushed_revs(root: Path) -> list[str] | None:
    """The commits this push would publish, from what git puts on stdin.

    Lines are '<local ref> <local sha> <remote ref> <remote sha>'. A remote sha
    of zeroes means the branch is new there - and on a first push to a new
    public remote that is the whole history, which is the case this exists for,
    so nothing is subtracted.

    Only ever called for --push. Deciding by isatty() instead would mean a
    manual run from anything that is not a terminal - a script, an editor, an
    assistant - sits waiting on a stdin that is never going to close.
    """
    revs = []
    for line in sys.stdin.read().splitlines():
        parts = line.split()
        if len(parts) == 4 and parts[1] != ZERO:
            revs.append(parts[1])
    return revs


def main() -> int:
    root = Path(git(Path.cwd(), "rev-parse", "--show-toplevel").strip())
    config = load_config(root)
    checks = build_checks(config)
    allow = [re.compile(p) for p in config["allow_patterns"]]

    if "--history" in sys.argv:
        revs = ["--all"]
    elif "--push" in sys.argv:
        revs = pushed_revs(root)
    elif "--range" in sys.argv:
        # What this change introduces, rather than everything the repository
        # has ever carried. A repository with a dirty history would otherwise
        # fail every pipeline for ever, and a check that is always red is one
        # nobody reads - but nothing new gets to land.
        try:
            spec = sys.argv[sys.argv.index("--range") + 1]
        except IndexError:
            print("--range needs a revision range, e.g. main..HEAD", file=sys.stderr)
            return 2
        revs = [spec]
        if not git(root, "rev-list", "--count", spec).strip().strip("0"):
            print(f"check-public: no commits in {spec}; nothing to check")
            return 0
    else:
        revs = None  # a manual run looks at the working tree

    if revs:
        # "history" is the right word for a push or a whole-repository scan; for
        # a range it is the change in front of you, and saying otherwise sends
        # people looking through old commits for a line they just wrote.
        prefix = "" if "--range" in sys.argv else "history "
        problems = scan_commits(root, revs, config, checks, allow, prefix)
        subject = ("This change would publish" if "--range" in sys.argv
                   else "This push would publish")
    else:
        problems = scan_worktree(root, config, checks, allow)
        subject = "This must not be published"

    if not problems:
        print("check-public: nothing here that should not be public")
        return 0

    seen, unique = set(), []
    for problem in problems:
        if problem not in seen:
            seen.add(problem)
            unique.append(problem)

    print(f"{subject}:\n", file=sys.stderr)
    for problem in unique[:100]:
        print(f"  {problem}", file=sys.stderr)
    if len(unique) > 100:
        print(f"  ... and {len(unique) - 100} more", file=sys.stderr)
    print(
        "\nNothing has been pushed. If one of these is genuinely fine, end the"
        "\nline with '# noqa: public', or settle the whole class of them in"
        "\n.check-public.json. To override in an emergency: git push --no-verify,"
        "\nand then look at what you overrode.",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
