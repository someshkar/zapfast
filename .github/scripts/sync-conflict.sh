#!/usr/bin/env bash
# Hands a conflicting replay to whoever can resolve it.
#
# The workflow runs this when sync-upstream.sh exits 3: the replay is on HEAD,
# part-done, with the conflict markers committed. Publishing that branch lets the
# resolution happen anywhere, and the issue says which files need it.
#
# Env: BRANCH        branch to publish the replay on
#      SUBJECT       the commit of ours that would not apply
#      FILES         the files left with markers
#      UPSTREAM_SHA  upstream's main at the time
#      GH_TOKEN      token for the issue and the push
set -euo pipefail

: "${BRANCH:?}" "${SUBJECT:?}" "${FILES:?}" "${UPSTREAM_SHA:?}"

git push --force-with-lease origin "HEAD:refs/heads/$BRANCH"

body=$(
    printf 'Upstream moved to `%s`, and the fork\x27s commit **%s** no longer\n' \
        "$UPSTREAM_SHA" "$SUBJECT"
    printf 'applies on its own.\n\n'
    printf 'The replay so far is on the `%s` branch, with the conflict markers\n' "$BRANCH"
    printf 'committed. To finish it:\n\n'
    printf '```sh\n'
    printf 'git fetch origin %s\n' "$BRANCH"
    printf 'git switch %s\n' "$BRANCH"
    printf '# resolve the markers in: %s\n' "$FILES"
    printf 'git commit --all\n'
    printf 'git push --force-with-lease origin HEAD:main\n'
    printf '```\n\n'
    printf 'Resolve it into the commit it belongs to (`git commit --amend`, or an\n'
    printf 'interactive rebase before pushing), because the next sync replays that same\n'
    printf 'commit and a resolution that lives only in `main` is replayed away tomorrow.\n'
    printf 'FORK.md has the longer version.\n'
)

gh label create upstream-sync-conflict \
    --description "The fork's commits no longer replay onto upstream" \
    --color d93f0b --force >/dev/null 2>&1 || true

number=$(gh issue list --label upstream-sync-conflict --state open \
    --json number --jq '.[0].number // empty')
if [ -n "$number" ]; then
    gh issue comment "$number" --body "$body"
    echo "commented on issue #$number"
else
    gh issue create \
        --title "Upstream sync needs a resolution" \
        --label upstream-sync-conflict \
        --body "$body"
fi
