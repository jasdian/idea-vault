# Intent — Claude CI hotfix: a failed CI run on main becomes an issue and a hotfix PR (ADR-0041)

A red CI run on main, such as a new stable clippy lint (run 36718191131), stays red until the owner
notices it. The new workflow `.github/workflows/claude-ci-hotfix.yml` runs on a failed `CI` run from
a push to main, or on a dispatch with a run id. Claude records the failure in a `ci-failure` issue
and, if main still fails, fixes the root cause on `hotfix/ci-<run_id>` in a `Fixes #N` PR. It works
under the no-mistakes guardrails (ADR-0041, docs/14-no-mistakes-gate.md). `claude-review.yml` also
reviews those hotfix PRs. The owner approved this design, and it is documented in
docs/10-testing-strategy.md.

## Acceptance criteria

- The workflow starts only for a failed `CI` run whose event is `push` on `main` in this repository.
  A dispatch is held to the same check. A pull-request CI run never starts it, so a failing hotfix
  PR cannot loop.
- There is one issue and one PR per failing sha: an open `ci-failure` issue or `hotfix/ci-*` PR
  naming the sha skips the run, and the job concurrency is grouped per sha.
- When main's CI is already green at a later commit, the run records the failure in an issue,
  closes it, and pushes no branch.
- Claude branches from current `origin/main` and re-runs CI's exact four commands on the same
  toolchain. It never pushes to main, force-pushes or merges, never adds a lint suppression or
  weakens a check, and gives a diagnosis only after 3 failed attempts. A deterministic post-step
  fails the job if main moved or the hotfix diff adds `#[allow]`/`#[expect]` or touches
  `.github/`, the gate or lint config.
- With `CI_HOTFIX_TOKEN` set, the PR starts CI. Without it, the PR and the issue say CI must be
  started by hand.
- `claude-review.yml` reviews `hotfix/ci-*` PRs authored by github-actions or the `CI_HOTFIX_BOT`
  app, as well as the owner's PRs; fork PRs stay excluded.
- `actionlint` (with shellcheck) passes on `.github/workflows/`, and `bash scripts/gate.sh` is
  green.

## Expectation changes

None: this change touches no fixture, snapshot, floor or invariant rule.
