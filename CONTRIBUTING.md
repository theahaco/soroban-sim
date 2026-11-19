# Contributing to soroban-sim

Build, test and architecture notes are in [README.md](./README.md) and
[docs/](./docs); releases, the vendored host and the protocol twin crates are in
[docs/releases.md](./docs/releases.md). This file covers how a change is
described.

## Pull requests and commits

soroban-sim merges by squash. The squash commit's subject is the PR title (or,
for a single-commit PR, that commit's subject), and its message is built from
the PR's commit messages. Release-plz then writes each crate's `CHANGELOG.md`
from the Conventional subjects on `main` (`feat` under Added, `fix` under
Fixed) and uses their types to choose the version bump. So the title, the
commits and the description are all read long after review: write them for
someone running `git log` or reading a changelog in a year.

### Title

`type(scope): imperative summary`, lower-case after the colon, no full stop,
72 characters at most ([Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/)).
Add `!` before the colon for a breaking change.

- **Types:** `feat`, `fix`, `perf`, `refactor`, `revert`, `test`, `docs`,
  `build`, `ci`, `chore`, `style`.
- **Scopes** (optional; comma-separate if a change truly spans two):
  - crates: `cli`, `core`, `facade`, `component`, `host` (the vendored
    `soroban-sim-host`), `preview`, `twins` (the `-p30` crates);
  - CLI areas: `session`, `replay`, `serve`, `snapshot`, `differential`;
  - packages: `soroban-sign-dialog`, `soroban-sim-vitest`, `web-demo`, `js`
    (`@theahaco/soroban-sim`);
  - the rest: `fixtures`, `release`, `pr-preview`, `readme`, `deps`.

The scope list lives in [`.github/review-conventions.json`](./.github/review-conventions.json).
Use a draft PR rather than "WIP" or "DO NOT MERGE" in the title.

### Description

GitHub opens a PR with the default template. Pick a variant when the change
fits one better, with `gh pr create --template <file>` or by adding
`?template=<file>` to the compare URL:

| Template | Use it for |
| --- | --- |
| [default](./.github/pull_request_template.md) | Most changes |
| [`fidelity.md`](./.github/PULL_REQUEST_TEMPLATE/fidelity.md) | Anything that can move a simulated result, fee or metering figure against the network: the vendored host, the engine, the facade, the component, the preview engine, a host version |
| [`mechanical.md`](./.github/PULL_REQUEST_TEMPLATE/mechanical.md) | Twin sync, the vendored host copy and `UPSTREAM.json`, package assets, lockfiles, recaptured fixtures, moved code |
| [`stack.md`](./.github/PULL_REQUEST_TEMPLATE/stack.md) | One PR in a stack |
| [`docs.md`](./.github/PULL_REQUEST_TEMPLATE/docs.md) | Documentation only |

The sections every template shares, and what each must contain:

- **Why:** the problem and why this approach. Link the issue here.
- **What changes:** what a CLI user, RPC client, browser integrator or signer
  sees differently, not a file list; then what deliberately does not change.
- **How to review:** where to start (`path::symbol`), the one to three risky
  places, and what to skim (with the command that generated it).
- **Evidence:** commands and results with counts, under both hosts (p29, p30)
  where it matters, a test that fails without the change, and what was not run.
- **Decisions for review:** the choices a reviewer could make differently,
  as questions.
- **After merge:** releases, docs, follow-ups, and who owns each.

The fidelity template adds two:

- **Fidelity impact:** which results can move (return values, events,
  footprints, fees and refunds, rent, CPU and memory metering, auth outcomes,
  RPC fields), on which host, and whether each moves toward the network.
  "None: <why>" is an answer when the change cannot move a result.
- **Network comparison:** what was replayed or compared and the result, with
  counts: the replay sample (`soroban-sim replay run ... --report fidelity`,
  [docs/replay.md](./docs/replay.md)), the parity fixtures, the
  [differential oracles](./docs/differential-testing.md), a live RPC.

The description is a record of the change, for its reviewer and for later
readers:

- Delete every `<!-- -->` comment before marking the PR ready.
- Answer a section that does not apply with "None." rather than deleting it.
- Keep the description true at merge time.
- Never paste in the request, task instructions, prompts, quoted chat or
  review discussion. They go in PR comments, if anywhere.

### Commits

Commits reach `main` through the squash message, so they follow the same
structure in miniature. Enable the template once per clone:

```sh
git config commit.template .gitmessage
```

[`.gitmessage`](./.gitmessage) gives the shape: a subject in the title's
format, then a body with a `Why:` paragraph (every commit type; one line is
enough for a small change), `What changes:` and `Evidence:` where they add
something, then trailers:

- `Refs: #123` (or `Closes: #123`) for issues.
- `Fidelity: <what can move, on which host>`, or `Fidelity: none`, on a
  commit that touches the host, the engine or the facade.
- `BREAKING-CHANGE: <what breaks and how to migrate>` with `!` in the
  subject. Write it in upper case: Conventional Commits tooling treats
  `BREAKING-CHANGE` and `BREAKING CHANGE` as the only spellings.

Give each commit one kind of risk: a move or rename, a behaviour-preserving
refactor, a behaviour change with its tests, regenerated output (name the
command), docs. A twin sync (`scripts/sync-protocol-twins.py`) belongs in the
commit that changes its source. Fold fixups into the commit they fix before
asking for review (`git commit --fixup=<sha>`, then
`git rebase -i --autosquash`).

Check messages locally before pushing, with the same shared checker CI runs
(Node 24):

```sh
rc="npx --yes github:bitswell-hq/review-conventions#v1"
$rc commits --base origin/main                      # this branch's commits
$rc body --body-file body.md --base origin/main     # a description
$rc title --title "fix(replay): ..."
```

Or as a `commit-msg` hook (`.git/hooks/commit-msg`):

```sh
exec npx --yes github:bitswell-hq/review-conventions#v1 commit-msg "$1" --mode block
```

### What CI checks

[`.github/workflows/review-conventions.yml`](./.github/workflows/review-conventions.yml)
runs on every PR open, push and edit. It calls the shared
[bitswell-hq/review-conventions](https://github.com/bitswell-hq/review-conventions)
workflow, whose README lists every rule:

- the title format, type and scope;
- leftover template comments, and missing or empty template sections;
- process headings: `## Intent`, `## Prompt`, `## Pipeline`, `## Agent log`,
  `## Review notes` and the like, or any heading carrying a date or a
  "round N";
- each commit message.

The last two are how instructions or conversation pasted into a description
get caught. Prose without such a heading cannot be told apart from a real
description, so the reviewer remains the check for it.

soroban-sim adds two rules in
[`.github/review-conventions.json`](./.github/review-conventions.json):

- `fidelity-sections`: `## Fidelity impact` and `## Network comparison` are
  required when the PR changes `crates/soroban-sim-host/`, the `src/` of
  `soroban-sim-core`, `-facade`, `-component` or `-preview`, or a `-p30`
  twin's `Cargo.toml` (where the protocol-30 host is pinned);
- `fidelity-none-reason`: a "None" in either section gives at least a short
  reason.

Release-plz PRs get only the title check: their body is written by
release-plz.

The checks **warn only**: findings show as annotations and in the job
summary, and the check passes. Changing `mode: warn` to `mode: block` in the
workflow makes any finding fail, except on a draft PR.

### Generated and vendored files

[`.gitattributes`](./.gitattributes) marks the vendored host sources
`linguist-vendored`, and marks `linguist-generated` the files a script or
tool writes: the twin crates' sources (`scripts/sync-protocol-twins.py`), the
crates' package copies of fixtures, WIT and session examples
(`scripts/sync-package-assets.py`), and lockfiles. GitHub collapses them in
the diff. If you edit one of those files by hand, list the edit under the
mechanical template's `## Hand edits`. Never mark a hand-written or captured
file as generated: a collapsed file escapes review.

### Where the rules live

The checker is shared with other repositories, so soroban-sim does not change
it here. A generic rule that is missing or wrong is fixed in
bitswell-hq/review-conventions. soroban-sim owns its templates under
`.github/`, `.gitmessage`, `.gitattributes`, and the scopes and rules in
`.github/review-conventions.json`.
