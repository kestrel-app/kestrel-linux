#!/usr/bin/env bash
# Publish this commit's tree to the public repository, as one commit.
#
#   tools/publish-public.sh https://github.com/you/repo.git [branch]
#
# What lands publicly is a single parentless commit containing exactly the tree
# of HEAD. The internal history stays on GitLab: a hostname that was removed
# three commits ago is not published, because there is no third commit to read
# it out of.
#
# This is what the manual publish job in .gitlab-ci.yml runs. It is the only
# path to the public repository - the automatic GitLab-to-GitHub mirror is
# deliberately switched off, because a mirror publishes when a push lands and
# so cannot be gated by anything, least of all a human.
#
# Identity, in order of preference: PUBLISH_NAME / PUBLISH_EMAIL from the
# environment, then publish.name / publish.email from git config. Never
# user.email, which on the machine this was written on is an internal address.
#
# Credentials, in CI: set GITHUB_TOKEN as a masked variable. It is written to a
# job-local credential file rather than put in the URL, so it cannot end up
# in the job log or the process list.
set -euo pipefail

remote="${1:-}"
branch="${2:-main}"
message="${PUBLISH_MESSAGE:-Release from ${CI_COMMIT_SHORT_SHA:-$(git rev-parse --short HEAD)}}"

if [ -z "$remote" ]; then
    echo "usage: tools/publish-public.sh <remote-url> [branch]" >&2
    exit 2
fi

root="$(git rev-parse --show-toplevel)"
cd "$root"

name="${PUBLISH_NAME:-$(git config --get publish.name || true)}"
mail="${PUBLISH_EMAIL:-$(git config --get publish.email || true)}"
if [ -z "$name" ] || [ -z "$mail" ]; then
    echo "No public identity. Set PUBLISH_NAME and PUBLISH_EMAIL, or:" >&2
    echo "  git config publish.name  \"Your Name\"" >&2
    echo "  git config publish.email \"1234+you@users.noreply.github.com\"" >&2
    echo >&2
    echo "Inheriting user.email here would publish an internal address." >&2
    exit 2
fi

# The gate. Nothing is built or pushed if this says no.
python3 "$root/tools/check-public.py"

# A push with no usable credential would otherwise ask for a username on a
# terminal that is not there and sit until the job times out.
export GIT_TERMINAL_PROMPT=0

# "https://github.com/.git" is what an unset GITHUB_REPO expands to.
case "$remote" in
    *github.com/.git|*github.com/|*//.git)
        echo "The remote is $remote, so GITHUB_REPO is unset or empty." >&2
        echo "Set it to owner/name in Settings > CI/CD > Variables." >&2
        exit 2 ;;
esac

if [ -z "${GITHUB_TOKEN:-}" ] && [ "${CI:-}" = "true" ]; then
    echo "GITHUB_TOKEN is not set, so this push has no credential." >&2
    echo "If it is set, check it is not marked Protected while this pipeline runs" >&2
    echo "on an unprotected branch, and that its environment scope includes it." >&2
    exit 2
fi

# What the published tree leaves out. .gitlab-ci.yml describes an internal
# pipeline - jobs, runners and variables that exist on one GitLab and nowhere
# else - and .gitignore describes the working layout of the machine this was
# built on. Neither is any use to somebody who cloned the public repository,
# and both invite questions about infrastructure that is not theirs.
#
# The removal happens in a scratch index, so HEAD, the working tree and the
# GitLab side all keep both files. Only the published snapshot is without them.
# A name that is not there is not an error: --force-remove ignores it.
exclude=(.gitignore .gitlab-ci.yml docs/internal.md)

scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT

# The credential lives in this job's scratch directory and nowhere else, and the
# push ignores the machine's git configuration altogether. On a shell-executor
# runner every project's jobs share one user: a credential helper, a stored
# credential or a url.<base>.insteadOf rewrite with a token in it, left there by
# or for another project, would otherwise decide who this push authenticates as
# ("Permission to ... denied to <other project>").
push_env=(GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1)
push_config=()
if [ -n "${GITHUB_TOKEN:-}" ]; then
    (umask 077; printf 'https://x-access-token:%s@github.com\n' "$GITHUB_TOKEN" \
        > "$scratch/credentials")
    push_config=(-c credential.helper= -c "credential.helper=store --file=$scratch/credentials")

    # Say whose token this is before using it, without printing it: the header
    # is read from a file so the token is not in curl's argv either.
    if command -v curl >/dev/null; then
        (umask 077; printf 'Authorization: Bearer %s\n' "$GITHUB_TOKEN" > "$scratch/auth")
        login="$(curl -sS -H @"$scratch/auth" https://api.github.com/user \
            | sed -n 's/^ *"login": *"\([^"]*\)".*/\1/p' | head -1 || true)"
        if [ -n "$login" ]; then
            echo "GITHUB_TOKEN belongs to GitHub user: $login"
        else
            echo "GITHUB_TOKEN is not a personal token (a GitHub App installation token" \
                 "or an invalid token); it must be able to push to $remote" >&2
        fi
    fi
fi
export GIT_INDEX_FILE="$scratch/index"
git read-tree HEAD
git update-index --force-remove -- "${exclude[@]}"
tree="$(git write-tree)"
unset GIT_INDEX_FILE

commit="$(
    GIT_AUTHOR_NAME="$name"    GIT_AUTHOR_EMAIL="$mail" \
    GIT_COMMITTER_NAME="$name" GIT_COMMITTER_EMAIL="$mail" \
    git commit-tree "$tree" -m "$message"
)"

echo "built $commit - one commit, no parents, authored as $name <$mail>"
echo "publishing to $remote ($branch)"

# --force because each publish is a fresh parentless commit, so it never
# fast-forwards from the last one. The public branch is a snapshot of what was
# released, not a history.
env "${push_env[@]}" git ${push_config[@]+"${push_config[@]}"} push --force "$remote" "$commit:refs/heads/$branch"

echo "published."
