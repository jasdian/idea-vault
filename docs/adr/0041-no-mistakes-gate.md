# ADR-0041 — The no-mistakes gate

- **Status:** Accepted
- **Date:** 2026-09-30
- **Deciders:** owner (decisions recorded 2026-09-30)
- **Amends:** [ADR-0030](./0030-gated-build-plan.md) (the run protocol's rules 9–12; still 12 steps)
- **Links:** [ADR-0034](./0034-grounded-ranked-and-bounded-workflow-stages.md) (the runtime CallBudget), [ADR-0038](./0038-parser-corpus-and-read-only-regrade.md) (the corpus replay)

## Context

`scripts/gate.sh` had six steps and its first was only `[ -s docs/INTENT.md ]`. `check-invariants.sh`
stopped at its first failing grep, had no rule ids, severities or catalog, and no test ran either
script, so a rule could rot or be quietly deleted. Nothing asked whether an expectation had been
*weakened to go green*: a fixture, a snapshot, a raised ratchet floor or a dropped rule. And the
findings protocol an executor follows when a check fails lived only in the developer's head, while
every build plan's `PROMPT.md` carried a protocol whose rule 10 ("stop after 3 failed attempts") said
how many tries but not what a fix may touch.

## Decision

We will make the gate a **fixed, flagless pipeline** with a collect-all invariant catalog, seed every
rule with a test, check for weakened expectations, and state one findings protocol on both sides. The
canonical text is [docs/14-no-mistakes-gate.md](../14-no-mistakes-gate.md); the flow is
[D41](../14-no-mistakes-gate.md).

- **Fixed pipeline, no skip.** `gate.sh` runs seven steps in order and stops at the first red one:
  intent, invariants (`--strict`), build, tests, fmt, clippy, honesty. There is no `--skip`, `--from`
  or environment seam. The only flags are `--list` (print the step table) and `--install-hook`
  (alone); an unknown flag, or a flag combined with another, exits 2 and lists the valid ones. After
  any fix, the whole gate re-runs from step 1: stricter than restarting at step 2, and step 1 is cheap.
- **Budget is a recorded non-step.** There is no metered spend (Ollama is local, claude runs on a
  subscription); the runtime cap is the workflow CallBudget
  ([ADR-0034](./0034-grounded-ranked-and-bounded-workflow-stages.md)). The header of `gate.sh` and
  docs/14 say so, so its absence is a decision, not an omission.
- **Step 1, intent.** The top `# Intent` block of `docs/INTENT.md` has `## Acceptance criteria` with at
  least one `- ` bullet and names an `ADR-NNNN` or `D<n>`. **Freshness:** on a branch other than
  `main`, `docs/INTENT.md` must differ from `merge-base(HEAD, main)`; with no `main` ref the step fails
  and names `git fetch origin main:main`, never a silent pass. Older intent blocks are archived in
  `docs/intent-archive.md`.
- **Step 4, tests.** `cargo test`, but it fails first if `PARSER_CORPUS_BLESS` is set at all: blessing
  the snapshot inside the gate is a bypass.
- **Step 7, honesty.** On a branch, the expectation surface changed since the merge-base is collected:
  changed or new files under `tests/fixtures` and `tests/**/*.snap`, rising `*_FLOOR=` lines in
  `check-invariants.sh`, and catalog ids removed or downgraded (a diff of `--list` at the base and at
  HEAD, skipped once when the base script has no `--list`). Each must be declared as
  `- <path or id>: <why>` under `## Expectation changes` in the top intent block (an item ending in
  `/` covers a directory), or the step is red. Then `cargo test --test parser_corpus` replays the
  corpus by name ([ADR-0038](./0038-parser-corpus-and-read-only-regrade.md)). **Known limit:**
  assertion edits inside `tests/*.rs` cannot be told from other edits mechanically; they stay an
  ask-user rule plus review.
- **Invariants collect all findings.** `check-invariants.sh [--strict] [--root <dir>] | --list`.
  Every rule in a fixed catalog runs; each reports `[ok] <id> — <zero-state> (<ADR/D>)` or one
  `ERROR|WARN|INFO <id>: <target> — <message>` line per finding. `--strict` promotes WARN to ERROR at
  emit time; INFO is never counted. Exit is 0 (no errors), 1 (any error) or 2 (usage). `--root` lets a
  test point the script at a temporary tree; `--list` prints `id|severity|ADR/D|zero-state`.
  The catalog's rules include the original nine (`ollama-url`, `bind-addr`, `restart-no`,
  `vault-bind-long`, `no-docker-exec`, `doc-links`, `ratchet`, `d4-config`, `d4-web-app`) and add
  `ratchet-slack` (a count below its floor: lower the floor in the same commit), `ignore-ratchet`,
  `doc-ranges` (CLAUDE.md's "D1–Dnn" and "ADRs 0001–NNNN" equal the highest diagram and ADR),
  `doc-range-gaps` (info), `checklist-mirror`, `intent-archive` (info), `tool-fence`,
  `no-skip-permissions` ([ADR-0039](./0039-foil-hygiene-and-lockdown.md)) and `runs-not-truth`
  ([ADR-0037](./0037-run-journal-diagnostics-only-call-record.md)).
- **A seed per rule arm.** `tests/gate_invariants.rs` builds the smallest clean tree, and a `SEEDS`
  table plants exactly one violation per catalog id and per detection arm of a multi-arm rule; each
  seed must flag exactly its own id. A meta-test asserts the `--list` ids equal the `SEEDS` ids, so
  a rule cannot ship without a seed, and another runs the versioned tree (tracked and unignored
  files, copied out, so gitignored `.claude/` state never decides it) with `--strict`. `tests/gate_script.rs` drives `gate.sh` in a temporary
  git repo with a stub `cargo` (no seam inside the script) for the hook, the intent checks, the bless
  refusal and step 7.
- **The hook contract.** `gate.sh --install-hook` resolves the hooks directory with
  `git rev-parse --git-path hooks` (honouring `core.hooksPath` and worktrees), writes `pre-push` with
  the marker `# idea-vault gate pre-push hook v1` at mode 0755, re-installs over its own hook, and
  refuses (exit 1) a hook without the marker. The hook runs
  `scripts/check-invariants.sh --strict`: fast, no cargo, and the same strictness as gate step 2.
- **Findings by action, not severity.** Act on a failure by what fixing it would change.
  *No-op*: note it and continue. *Auto-fix*: at most 3 attempts per gate step; commit each fix that
  greens a red step as `gate(<step>): <summary>`; re-run the whole gate. *Ask-user*: 0 attempts,
  relay the finding verbatim; anything touching `docs/INTENT.md` acceptance, an ADR's Decision
  section, CLAUDE.md "Confirmed architecture decisions", an existing test's assertions, a fixture,
  snapshot or golden, a rising floor, or removing or downgrading an invariant rule is ask-user.
  Hard rules: never `--no-verify`; never set `PARSER_CORPUS_BLESS` to get green; never raise a floor
  or a cap mid-gate; never judge by piped output (CORE-1); never commit around a red gate.
- **Product side.** `RUN_PROTOCOL` in `build_plan::plan` keeps 12 rules (ADR-0030's "12 steps" stays
  true) and rewrites 9–12 as the same protocol: one fixed pipeline per task; act by no-op, auto-fix
  or ask-user; never weaken a check; end each report with findings, and encode an escaped mistake.
  `FIELD_ACTION` classifies every `FIELD_KEYS` entry (`reads` is a no-op; every other key, every
  derived and every owner key is ask-user; a test fails if a key is unclassified) and `INTENT_SECTIONS`
  names the plan sections that are intent. plan.md's header reads `Rules: PROMPT.md (How to run this and its
  rule 10 Findings, PINNED, Fence)`: the findings protocol is rule 10 of `## How to run this`, not a
  section of its own, and a test holds every name in the header to what PROMPT.md contains.
- **Encode the class, no ledger.** An escaped dev-process mistake becomes, in order of preference, a
  new invariant rule plus its `SEEDS` row, a new test, or a `[dev]` checklist line in docs/14 mirrored
  in `.claude/skills/attack/SKILL.md` and `.claude/rules/core.md`, which `scripts/skill-check.sh`
  (the `checklist-mirror` rule) keeps verbatim. It is not recorded as a memory file; memory keeps
  preferences and context. `[product]` checklist phrases must appear verbatim in `RUN_PROTOCOL`,
  checked by `plan::tests`.

## Consequences

- **Every red is reported at once.** A run lists every invariant finding instead of one at a time.
- **A rule cannot silently rot.** A rule with no seed fails the meta-test, and removing or downgrading
  one is an expectation change that must be declared.
- **Weakening is visible.** Changing a fixture, snapshot, floor or the catalog needs a stated reason
  in the intent block, so a green gate says more than "the checks I left were passing".
- **Friction the owner accepted.** Every fixture, snapshot or floor change is declared; an unchanged
  `docs/INTENT.md` on a branch fails step 1; blessing the corpus is an ask-user act.
- **The `.claude/` mirrors are not versioned.** `.claude/` is gitignored, so the `checklist-mirror`
  rule is INFO for a mirror file that is absent (a fresh clone, or a worktree whose `.claude/` holds
  only a campaign workspace) and an error for one that is present and drifted.
- **Honesty runs on main too.** Only step 1's freshness is skipped on `main`; step 7 diffs the
  working tree and index against `HEAD` there, so committing straight to `main` cannot bypass it.
  The canonical copy is docs/14.
- **`regrade --strict` stays outside the gate** because it needs a vault; the corpus replay is its
  offline stand-in.
- **A tightened range check.** CLAUDE.md's diagram and ADR ranges must be updated in the same change
  that adds a diagram or ADR.

## Alternatives considered

- **A configurable gate (`--skip`, `--from`, env seams).** Rejected: every seam is a way to go green
  without running a step.
- **Restart at step 2 after a fix.** Rejected: step 1 is cheap and a fix can touch the intent.
- **A budget step.** Rejected: there is nothing metered to cap; the runtime cap is CallBudget.
- **An LLM judge in the gate.** Rejected: a gate that a model can talk around is not a gate
  ([ADR-0023](./0023-verification-layer.md) rejects a model judging grounding for the same reason).
- **Record mistake classes as memory.** Rejected by the owner: a memory is advice, a rule is
  enforcement. Existing mistake memories migrate into rules and tests over time; they are not deleted.
- **Wire `regrade --strict` into the gate.** Rejected: it needs the owner's vault.
- **Classify `depends` as auto-fix or `reads` as ask-user.** Rejected by the owner: `depends` changes
  the task graph (ask-user); `reads` is an advisory "open first" hint (no-op).

---

> ADRs are immutable once **Accepted**. To change a decision, write a new ADR that supersedes this
> one and update the Status line above.
