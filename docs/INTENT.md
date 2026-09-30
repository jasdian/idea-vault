# Intent — Claude CI hotfix: a failed CI run on main becomes an issue and a hotfix PR (ADR-0041)

A red CI run on main, such as a new stable clippy lint (run 36718191131), stays red until the owner
notices it. The new workflow `.github/workflows/claude-ci-hotfix.yml` runs on a failed `CI` run from
a push to main, or on a dispatch with a run id. The workflow files a `ci-failure` issue. Claude,
holding no write access, diagnoses the failure and, if main still fails, fixes the root cause. A
deterministic job then guards the diff and opens a `Fixes #N` PR on `hotfix/ci-<run_id>`. It works
under the no-mistakes guardrails (ADR-0041, docs/14-no-mistakes-gate.md). `claude-review.yml` also
reviews those hotfix PRs. The owner approved this design, and it is documented in
docs/10-testing-strategy.md.

## Acceptance criteria

- The workflow starts only for a failed `CI` run whose event is `push` on `main` in this repository.
  A dispatch is held to the same check. A pull-request CI run never starts it, so a failing hotfix
  PR cannot loop.
- There is one issue per failing sha, found by a marker the workflow writes, in a serialised triage
  job; one hotfix is in flight at a time; at most 3 hotfix issues are filed per 24 hours; and a
  skipped run names the reason in its summary.
- When main's CI is already green at a later commit, the run records the failure in an issue,
  closes it, and pushes no branch.
- Claude runs with a read-only token, no git or gh tools, unpersisted checkout credentials and a
  restore-only cache, on current main with CI's toolchain and four commands. It gives a diagnosis
  only after 3 failed attempts. A deterministic publish job, which has the write token, refuses
  output that contains a secret and runs the guard before pushing. The guard withholds lint
  suppressions, `#[ignore]`, removed tests, `Cargo.toml` lint changes, symlinks and edits to
  `.github/`, `scripts/`, `CLAUDE.md` or build config. It opens a fix that touches fixtures,
  snapshots, floors or Cargo files as a draft. Only `hotfix/ci-<run_id>` is ever pushed, never
  forced, and nothing merges.
- With `CI_HOTFIX_TOKEN` set, the PR starts CI. Without it, the PR and the issue say CI must be
  started by hand, and a PR that GitHub refuses is reported on the issue with the setting to
  enable.
- `claude-review.yml` reviews `hotfix/ci-*` PRs authored by the one hotfix bot (`CI_HOTFIX_BOT`
  or github-actions), with advisory framing and `allowed_bots` set only for them, as well as the
  owner's PRs; fork PRs stay excluded. Every action in both Claude workflows is pinned to a
  commit SHA.
- `actionlint` (with shellcheck) passes on `.github/workflows/`, and `bash scripts/gate.sh` is
  green.

## Expectation changes

None: this change touches no fixture, snapshot, floor or invariant rule.
