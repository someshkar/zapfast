# This fork

This is [crmne/zapfast](https://github.com/crmne/zapfast) plus one feature, so
that a released binary can be installed on the machines here without waiting for
upstream. Upstream moves fast; the fork stays a day behind it at most.

## What the fork adds

| Change | Why |
|---|---|
| **Link previews for the links you send** | Upstream draws the card WhatsApp sends with a message but builds none for an outgoing link. The composer here reads the page behind a link in your own draft and sends its title, description and picture with the message, as the phone does. Settings → Chats → Preview links turns it off. Requested upstream in [#567](https://github.com/crmne/zapfast/issues/567), where the maintainer asked for exactly this shape. |
| `src/updates.rs` points at this fork | Upstream would otherwise install over the fork and take the previews away. |
| `scripts/fork-release.sh` | The binaries are built here and on the MacBook, not on CI runners. |

Everything else is upstream's, unchanged.

## How `main` is kept

`main` is upstream's history with the fork's commits replayed on top.

```
upstream/main ──●──●──●──●───────────●   (crmne/zapfast)
                            ╲
                             ●──●   fork's commits, replayed
```

[`.github/workflows/upstream-sync.yml`](.github/workflows/upstream-sync.yml) runs
once a day, and on demand, through
[`.github/scripts/sync-upstream.sh`](.github/scripts/sync-upstream.sh). The
script fetches upstream's `main`, takes every commit of the fork's that upstream
does not have, re-applies them one at a time, checks that the result still builds
and still carries the previews, and force-pushes `main`. Nothing is merged and
nothing is squashed, so `git log` reads upstream's commits first and the fork's
after them.

Upstream's release, board and released-fixes workflows are dropped on every
sync: they sign with credentials this fork does not have, and two of them call a
reusable workflow in the maintainer's repository. Dropping them on each run also
drops the ones upstream adds later, which a one-off commit cannot do. They are
listed in `DROP_WORKFLOWS` at the top of the script.

### The one thing to set up

The replay carries upstream's `.github/workflows` changes, and only a token with
the **Workflows** permission may publish those — the default `GITHUB_TOKEN`
cannot, so every write step fails with `refusing to allow a GitHub App to create
or update workflow`.

Create a fine-grained token (Settings → Developer settings → Personal access
tokens → Fine-grained), limited to this repository, with:

| Permission | Access |
|---|---|
| Contents | Read and write |
| Workflows | Read and write |
| Actions | Read and write |
| Metadata | Read (added automatically) |

Save it as the `SYNC_TOKEN` secret (Settings → Secrets and variables → Actions).
The workflow prefers it and falls back to `GITHUB_TOKEN`, which can still run the
replay and report, but cannot publish. With `SYNC_TOKEN` the push to `main` also
starts CI by itself.

## Adding a change to the fork

Commit it to `main` as usual, ideally one commit per change: a commit is the
unit the replay works on, so a small commit is a small conflict. The next sync
replays it, and if its resolution needed a decision, that decision stays in the
commit.

## When a replay conflicts

Upstream and the fork changed the same lines. The workflow pushes nothing to
`main`. It publishes the replay so far as `upstream-sync/next`, with the conflict
markers committed, writes the files to resolve into the run summary (issues are
disabled on this fork), and fails the run red.

```sh
git fetch origin upstream-sync/next
git switch upstream-sync/next
# resolve the markers; the run summary and the issue name the files
git commit --all
git push --force-with-lease origin HEAD:main
```

Resolve it into the commit it belongs to — `git commit --amend`, or an
interactive rebase on the branch first — because the next sync replays that same
commit, and a resolution that lives only in `main` is replayed away tomorrow.

## Upstreaming the previews

`link-previews` is the same change as one commit on top of `upstream/main`, with
none of the fork's plumbing in it: no `src/updates.rs`, no
`scripts/fork-release.sh`, no workflow files, no README blurb. That is the branch
the pull request to `crmne/zapfast` comes from, and what CONTRIBUTING.md asks
for: one focused change, squash-merged, catalogs untouched. The body of that pull
request lives outside the repository, at `~/projects/zapfast-pr/body.md`.

Its captures come from that branch, over the synthetic demo data, so no
conversation of anyone's is in them:

```sh
cargo run --features demo -- --demo-page link-preview,light --demo-shot after.png
cargo run --features demo -- --demo-page link-preview-off,light --demo-shot before.png
```

`link-preview-off` leaves the setting that builds the card switched off, which
draws the link as `main` draws it today, so the pair differs only by the card.

If upstream merges the previews, the fork's job for them ends: drop the link
preview commits from `main` and keep the updater and the release script. The sync
replays whatever `main` carries, so removing them is an ordinary commit.

## Releasing

The workflow checks that the replayed tree builds, but the binaries on the
releases page are built from `main` on our own machines:

```sh
scripts/fork-release.sh vX.Y.Z-fork.N
```

Linux is built on this machine, macOS arm64 and Windows x86_64 on the MacBook
over SSH. The tag sorts under upstream's (`v0.20.1-fork.1` follows `v0.20.1`),
and the fork's updater compares versions with the fork's own repository, so an
installation here never sees an upstream release.
