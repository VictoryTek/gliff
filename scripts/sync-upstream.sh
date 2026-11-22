#!/usr/bin/env bash
# Merge the upstream project's main into a new branch, for a pull request.
#
#   scripts/sync-upstream.sh [--summary FILE] [--dry-run]
#
# Fetches upstream (the `upstream` remote, added from $UPSTREAM_URL or
# omacom/gliff when it is missing), and, when upstream/main has commits we do
# not, merges it into a new branch `sync/upstream-<short sha>` made from the
# current HEAD. The merge happens in a temporary worktree, so the working tree
# you run this in is never touched. Nothing is pushed.
#
# It also decides whether the change needs a matching change in the GNOME
# backend: upstream touching the Hyprland-only crates, or the few places that
# call Hyprland directly. Edit NEEDS_MIRROR below when that list changes.
#
# Exit codes: 0 done (merged, or nothing to do, or the branch was already
# made); 2 bad use; 3 the merge conflicts (the conflicting files are in the
# summary).
#
# Results go to stdout, to --summary FILE (the pull request text) and, when
# $GITHUB_OUTPUT is set, to it: changed, branch, needs_mirror, result.
# The backticks in the sed scripts below are literal (markdown).
# shellcheck disable=SC2016
set -euo pipefail

UPSTREAM_URL="${UPSTREAM_URL:-https://github.com/omacom/gliff.git}"
UPSTREAM_BRANCH="${UPSTREAM_BRANCH:-main}"

# Files whose upstream changes need a look at the GNOME side. A clean merge
# does not mean the GNOME backend still works: a new direct compositor call, a
# changed type the gnome-* crates share with hypr-*, or a new protocol message
# the backend must act on all merge without a conflict. So whole crates are
# listed, not file names: the Hyprland crates, the server, the viewer, the
# probe, the protocol, the Vulkan crate (it holds our dmabuf code), and the
# workspace manifests (see docs/PLAN.md, "Upstream sync").
NEEDS_MIRROR='^crates/(hypr-[^/]+|gliff-server|gliff|gliff-probe|gliff-proto|gliff-vk)/|^Cargo\.(toml|lock)$'

summary=""
dry_run=0
while [ $# -gt 0 ]; do
    case "$1" in
        --summary) summary="${2:?--summary needs a file}"; shift 2 ;;
        --dry-run) dry_run=1; shift ;;
        *) echo "usage: $0 [--summary FILE] [--dry-run]" >&2; exit 2 ;;
    esac
done

out() { [ -z "${GITHUB_OUTPUT:-}" ] || echo "$1=$2" >> "$GITHUB_OUTPUT"; }

if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
    echo "the working tree has uncommitted changes" >&2
    exit 2
fi

if [ "$(git rev-parse --is-shallow-repository)" = true ]; then
    echo "this is a shallow clone; run 'git fetch --unshallow' first" >&2
    exit 2
fi

git remote get-url upstream >/dev/null 2>&1 || git remote add upstream "$UPSTREAM_URL"
git fetch --quiet upstream "$UPSTREAM_BRANCH"
tip=$(git rev-parse "upstream/$UPSTREAM_BRANCH")
short=$(git rev-parse --short=8 "$tip")
branch="sync/upstream-$short"

if git merge-base --is-ancestor "$tip" HEAD; then
    echo "up to date: HEAD already has upstream/$UPSTREAM_BRANCH ($short)"
    out changed false
    out result up-to-date
    exit 0
fi
if git show-ref --verify --quiet "refs/heads/$branch" \
    || git show-ref --verify --quiet "refs/remotes/origin/$branch"; then
    echo "already made: $branch exists"
    out changed false
    out result already-made
    exit 0
fi

base=$(git merge-base HEAD "$tip")
commits=$(git log --oneline --no-decorate "$base..$tip")
files=$(git diff --name-only "$base" "$tip")
mirror=$(printf '%s\n' "$files" | grep -E "$NEEDS_MIRROR" || true)
workflows=$(printf '%s\n' "$files" | grep -E '^\.github/workflows/' || true)

# The branch is ours to delete on any exit, unless the run ends well and is
# not a dry run.
keep=0
work=$(mktemp -d)
cleanup() {
    git worktree remove --force "$work" >/dev/null 2>&1 || true
    rm -rf "$work"
    [ "$keep" = 1 ] || git branch -D "$branch" >/dev/null 2>&1 || true
}
trap cleanup EXIT
git worktree add --quiet -b "$branch" "$work" HEAD

if ! git -C "$work" \
    -c user.name="${GIT_AUTHOR_NAME:-gliff upstream sync}" \
    -c user.email="${GIT_AUTHOR_EMAIL:-noreply@users.noreply.github.com}" \
    merge --no-ff --quiet -m "Merge upstream/$UPSTREAM_BRANCH ($short)" "$tip" >/dev/null 2>&1; then
    conflicts=$(git -C "$work" diff --name-only --diff-filter=U)
    git -C "$work" merge --abort >/dev/null 2>&1 || true
    text="Merging upstream/$UPSTREAM_BRANCH ($short) into HEAD conflicts.

Conflicting files:

$(printf '%s\n' "$conflicts" | sed 's/^/- `/; s/$/`/')

Upstream commits not in this branch:

$(printf '%s\n' "$commits" | sed 's/^/- /')"
    echo "$text"
    [ -z "$summary" ] || printf '%s\n' "$text" > "$summary"
    out changed false
    out result conflict
    exit 3
fi

text="Merges upstream/$UPSTREAM_BRANCH at \`$short\` into main.

Upstream commits ($(printf '%s\n' "$commits" | wc -l)):

$(printf '%s\n' "$commits" | sed 's/^/- /')

Files upstream changed: $(printf '%s\n' "$files" | wc -l)"
if [ -n "$mirror" ]; then
    text="$text

**Needs a look at the GNOME side.** Upstream changed files that call Hyprland directly or that the GNOME backend mirrors:

$(printf '%s\n' "$mirror" | sed 's/^/- `/; s/$/`/')

Check that \`crates/gnome-*\`, \`crates/gnome-backend\` and the \`Backend\` trait (\`crates/gliff-backend\`) still match, and add what is missing before merging."
fi
if [ -n "$workflows" ]; then
    text="$text

**Upstream changed workflow files**, which the workflow's own token cannot push: ${workflows//$'\n'/, }. Set the repository secret \`SYNC_TOKEN\` (a token with the \`workflow\` scope) or merge by hand."
fi

echo "$text"
[ -z "$summary" ] || printf '%s\n' "$text" > "$summary"
out changed true
out branch "$branch"
out needs_mirror "$([ -n "$mirror" ] && echo true || echo false)"
out touches_workflows "$([ -n "$workflows" ] && echo true || echo false)"
out result merged

if [ "$dry_run" = 1 ]; then
    echo "(dry run: branch $branch not kept)"
else
    keep=1
fi
