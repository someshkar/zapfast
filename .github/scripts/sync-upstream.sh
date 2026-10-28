#!/usr/bin/env bash
# Replays this fork's own commits on top of upstream's main.
#
# `main` is upstream's history with the fork's commits replayed on top: the link
# previews the sender's own machine builds, the self-update that points at this
# fork, and the release script that builds the binaries on our machines. Nothing
# is merged and nothing is squashed, so `git log` reads upstream's commits first
# and ours after them, and the fork never diverges into a second history.
#
# "The fork's commits" are the ones only this fork has: everything reachable
# from FORK_REF that upstream does not have. Add a change to the fork by
# committing it to `main` as usual; the next sync replays it. Keep each commit
# to one change, as upstream's CONTRIBUTING.md asks, because that is also what
# makes a conflicting replay small enough to resolve.
#
# A commit of ours whose every change is to one of the dropped workflows below
# is not replayed. Those removals are applied by this script on every run, so
# the release and triage workflows upstream adds later are dropped too; a
# one-off commit can only remove the files it knew about, and one that removes
# a file upstream keeps editing fails to replay on most runs.
#
# Usage: .github/scripts/sync-upstream.sh
#        (with `origin` and `upstream` fetched; see the workflow that calls it)
#
# Env:   UPSTREAM_REF   upstream's main as fetched locally   (default upstream/main)
#        FORK_REF       the fork's main to read commits from (default origin/main)
#        WORK_BRANCH    branch the replay is built on        (default upstream-sync/next)
#        DROP_WORKFLOWS names under .github/workflows not to carry
#
# Exit:  0 replayed, or already up to date
#        3 could not replay: WORK_BRANCH holds the commits replayed so far and a
#          commit with the conflict markers; resolve them and push main
#        4 the replay lost the fork's own changes; nothing was published
set -euo pipefail

upstream_ref=${UPSTREAM_REF:-upstream/main}
fork_ref=${FORK_REF:-origin/main}
work_branch=${WORK_BRANCH:-upstream-sync/next}
drop_workflows=${DROP_WORKFLOWS:-release.yml board.yml released-fixes.yml}

# Every value goes to the workflow through GITHUB_OUTPUT, and to the log for
# whoever reads the run afterwards.
out() {
    if [ -n "${GITHUB_OUTPUT:-}" ]; then printf '%s=%s\n' "$1" "$2" >>"$GITHUB_OUTPUT"; fi
    printf '  %-16s %s\n' "$1" "$2"
}

git config user.name >/dev/null 2>&1 || git config user.name "upstream-sync"
git config user.email >/dev/null 2>&1 || git config user.email "upstream-sync@users.noreply.github.com"

git rev-parse --verify --quiet "$upstream_ref^{commit}" >/dev/null || {
    echo "no $upstream_ref: add the upstream remote and fetch it first" >&2
    exit 2
}

upstream_sha=$(git rev-parse "$upstream_ref^{commit}")
out upstream_sha "${upstream_sha:0:12}"

if git merge-base --is-ancestor "$upstream_sha" "$fork_ref"; then
    out changed false
    echo "$fork_ref already contains $upstream_ref; nothing to do"
    exit 0
fi

base=$(git merge-base "$fork_ref" "$upstream_sha")
out upstream_commits "$(git rev-list --count "$base..$upstream_sha")"
out fork_commits "$(git rev-list --count "$base..$fork_ref")"

# A commit of ours that only removes (or edits) a workflow this fork does not
# carry is served by the pruning below, and replaying it would fight upstream
# every time upstream touches the same file.
only_dropped_workflows() {
    local commit=$1 path name
    [ -n "$(git diff --name-only "$commit^" "$commit")" ] || return 1
    while read -r path; do
        [ "$(dirname "$path")" = .github/workflows ] || return 1
        name=$(basename "$path")
        case " $drop_workflows " in
        *" $name "*) ;;
        *) return 1 ;;
        esac
    done < <(git diff --name-only "$commit^" "$commit")
    return 0
}

git switch --quiet --force-create "$work_branch" "$upstream_sha"

replayed=0
skipped=0
while read -r commit; do
    subject=$(git log -1 --format=%s "$commit")
    if only_dropped_workflows "$commit"; then
        skipped=$((skipped + 1))
        printf 'skip     %s\n' "$subject"
        continue
    fi
    if git cherry-pick --no-gpg-sign -x "$commit" >/dev/null 2>&1; then
        replayed=$((replayed + 1))
        printf 'replay   %s\n' "$subject"
        continue
    fi

    mapfile -t conflict_files < <(git diff --name-only --diff-filter=U)
    if [ "${#conflict_files[@]}" -eq 0 ]; then
        # Nothing conflicts, so either `rerere` finished the merge from an
        # earlier resolution or upstream has taken this change already and the
        # pick is empty. Finish it if git will, drop it if it will not.
        if git -c core.editor=true cherry-pick --continue >/dev/null 2>&1; then
            replayed=$((replayed + 1))
            printf 'replay   %s (merged by rerere)\n' "$subject"
        elif git cherry-pick --skip >/dev/null 2>&1; then
            skipped=$((skipped + 1))
            printf 'skip     %s (upstream has it)\n' "$subject"
        else
            echo "could not replay $commit and nothing conflicts; inspect by hand" >&2
            git cherry-pick --abort >/dev/null 2>&1 || true
            exit 1
        fi
        continue
    fi
    printf 'conflict %s\n' "$subject" >&2
    printf '         %s\n' "${conflict_files[@]}" >&2
    git add -A
    git commit --quiet --allow-empty \
        -m "Conflicting replay of \"${subject}\"" \
        -m "Upstream ${upstream_sha:0:12} changed these files under a commit of
ours. Resolve the markers, then:

    git push --force-with-lease origin HEAD:main

Files: ${conflict_files[*]}" 
    out changed conflict
    out conflict_subject "$subject"
    out conflict_commit "${commit:0:12}"
    out conflict_files "${conflict_files[*]}"
    exit 3
done < <(git rev-list --reverse "$base..$fork_ref")

out replayed "$replayed"
out skipped "$skipped"

# Upstream's release workflow signs with Apple and Azure credentials a fork does
# not have, and a `v*-fork.N` tag would start it anyway; its board and
# released-fixes workflows call a reusable workflow in the maintainer's own
# repository. None of them can run here, so the fork does not carry them.
pruned=()
for name in $drop_workflows; do
    path=".github/workflows/$name"
    if git ls-files --error-unmatch "$path" >/dev/null 2>&1; then
        git rm --quiet "$path"
        pruned+=("$name")
    fi
done
if [ "${#pruned[@]}" -gt 0 ]; then
    git commit --quiet -m "Keep upstream's release and triage workflows out of the fork"
    out pruned "${pruned[*]}"
fi

# The replay is only useful if the fork's own changes are still in it. A
# resolution that drops a hunk, or a rebase that skips a commit, would publish a
# fork without the feature it exists for.
missing=()
[ -f src/link_preview.rs ] || missing+=("src/link_preview.rs")
grep -q 'preview_links' src/settings.rs || missing+=("settings: preview_links")
grep -q 'sync_composer_preview' src/app.rs || missing+=("app: sync_composer_preview")
grep -q 'preview_strip' src/ui/conversation.rs || missing+=("conversation: preview_strip")
grep -q 'someshkar/zapfast' src/updates.rs || missing+=("updates: fork repository")
[ -x scripts/fork-release.sh ] || missing+=("scripts/fork-release.sh")
if [ "${#missing[@]}" -gt 0 ]; then
    printf 'the replay is missing the fork own changes: %s\n' "${missing[*]}" >&2
    out changed false
    out missing "${missing[*]}"
    exit 4
fi

out changed true
out head_sha "$(git rev-parse --short HEAD)"
