# Intent — a green merge does not run the CI gate again (issue #12, ADR-0041, D41)

A pull request's CI runs the whole no-mistakes gate on its merge commit, and the push to main that
merges it ran the same gate again on byte-identical files whenever main had not moved under the
PR: one full compile and test run per merge, for nothing. This change keeps the push-to-main run
(it is the hotfix trigger and catches a semantic conflict when main moved) but lets it skip the
gate when its commit's tree already passed the gate on a pull request. A green pull-request run
uploads a `gate-green-<tree sha>` artifact; the push run's first step looks for that artifact and,
if a successful same-repository `pull_request` run of `ci.yml` uploaded it and the push leaves
`.github/workflows/` untouched, skips the remaining steps. docs/10, docs/14 and CLAUDE.md describe the skip. The gate script itself is not edited.

## Acceptance criteria

- A pull request whose gate is green uploads one artifact named `gate-green-<tree>`, where `<tree>`
  is `git rev-parse 'HEAD^{tree}'` of the PR merge commit; a red gate uploads none.
- A push to main whose commit tree equals a green, unexpired, same-repository `pull_request` run's
  marker finishes CI green after only the checkout (no toolchain, cache, fetch or gate), and its
  step summary links the pull-request run.
- Any other push to main (one that changes `.github/workflows/`, since a PR that edits the
  workflow could drop the gate from its own run; no marker, an expired marker, a marker from a fork
  run or a run that is not a successful `pull_request` run of `ci.yml`; or any API error) runs the
  whole gate as before. The skip step itself never fails and logs why it did not skip.
- `actionlint` (with shellcheck) is clean on `.github/workflows/`, the new action is pinned to a
  commit, and `bash scripts/gate.sh` is green locally.

## Expectation changes

None: this change touches no fixture, snapshot, floor or invariant rule.
