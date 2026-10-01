# Intent — CI runs the no-mistakes gate: one job, no wasted runs, pinned actions (ADR-0041, D41)

Pull-request CI ran four cargo commands in two jobs (gate steps 3 to 6), so a green check said
nothing about intent, the invariant catalog, `validate` on the golden vault or honesty, and CI had
passed pull requests that fail `scripts/gate.sh`. A push to main re-ran the same four commands,
and every CI completion started a Claude review run that its job `if` then skipped. The two jobs
each checked out, installed and compiled the crate, and `ci.yml` pinned no action. This change
makes CI the gate (ADR-0041, D41): one job that runs `cargo fetch --locked` and then
`bash scripts/gate.sh` unchanged, on the PR merge ref with a local `main` ref for step 1; the
review workflow ignores push-to-main runs; the hotfix diagnose job restores the same cache key;
every action in `ci.yml` is pinned to a commit; and docs/10, docs/14 and CLAUDE.md describe what
CI runs. The gate script itself is not edited.

## Acceptance criteria

- A pull request's CI is one job whose log carries every gate step `1/7` … `7/7` and
  `validate: N idea(s), 0 finding(s)` with N > 0.
- A pull request that leaves `docs/INTENT.md` unchanged, or adds an unlisted fixture change, goes
  red in CI (checked on a throwaway branch, then deleted).
- A push to main runs CI once and starts no Claude review run.
- A stale `Cargo.lock` fails CI.
- The hotfix `diagnose` job's restore-only cache hits the key CI saves (`shared-key`).
- `actionlint` (with shellcheck) is clean on `.github/workflows/`, and `bash scripts/gate.sh` is
  green locally.

## Expectation changes

None: this change touches no fixture, snapshot, floor or invariant rule.
