# 14 — The no-mistakes gate

> The shipping gate (`scripts/gate.sh`), its invariant catalog (`scripts/check-invariants.sh`) and
> the findings protocol that both the developer and every build plan's `PROMPT.md` follow. Home of
> **D41**. Decision record: ADR-0041 (amends ADR-0030's run protocol, links ADR-0034 and ADR-0038).

## The pipeline

`bash scripts/gate.sh` runs seven steps in a fixed order and stops at the first red one. There is
no `--skip`, `--from` or environment seam; the only flags are `--list` (print this table) and
`--install-hook` (alone). After any fix, the whole gate re-runs from step 1.

| # | Step | What it checks |
|---|---|---|
| 1 | intent | The top `# Intent` block of `docs/INTENT.md` has `## Acceptance criteria` with a `- ` bullet and names an `ADR-NNNN` or `D<n>`; on a branch, `docs/INTENT.md` changed since `merge-base(HEAD, main)` (no `main` ref is red, never a silent pass) |
| 2 | invariants | `scripts/check-invariants.sh --strict` |
| 3 | build | `cargo build` |
| 4 | tests | `cargo test`; red first if `PARSER_CORPUS_BLESS` is set at all, because blessing inside the gate is a bypass |
| 5 | fmt | `cargo fmt --check` |
| 6 | clippy | `cargo clippy --all-targets -- -D warnings` |
| 7 | honesty | On a branch, every changed fixture or snapshot, rising `*_FLOOR`, and removed or downgraded catalog id since the merge-base is listed as `- <path or id>: <why>` under `## Expectation changes` in the top intent block (an item ending in `/` covers a directory) |
| — | budget | Not a step: there is no metered spend (Ollama is local, claude runs on a subscription); the runtime cap is the workflow CallBudget (ADR-0034) |

`--install-hook` writes a `pre-push` hook (marker `# idea-vault gate pre-push hook v1`, mode
0755) into `git rev-parse --git-path hooks`, which honours `core.hooksPath` and worktrees. It
re-installs over its own hook and refuses one without the marker. The hook runs
`scripts/check-invariants.sh --strict`: fast, and no cargo.

## The invariant catalog

`scripts/check-invariants.sh --list` prints it as `id|severity|ADR/D|zero-state`. Every rule runs
and reports (`[ok] <id> — …` or `ERROR|WARN|INFO <id>: <target> — <message>`); `--strict` promotes
WARN to ERROR; INFO never counts. Every id has a seeded violation in `tests/gate_invariants.rs`
that flags exactly that id, and a meta-test fails when a rule ships without one.

## Findings protocol

Act on a finding by what fixing it would change, not by how bad it looks.

- **no-op** — a note that changes no file: record it and continue.
- **auto-fix** — at most 3 attempts per gate step; a fix that greens a red step is committed as
  `gate(<step>): <summary>`; re-run the whole gate after every fix.
- **ask-user** — 0 attempts; relay the finding verbatim. Anything that touches `docs/INTENT.md`
  acceptance, an ADR's Decision section, CLAUDE.md "Confirmed architecture decisions", an existing
  test's assertions, a fixture, snapshot or golden, a rising floor, or removes or downgrades an
  invariant rule is ask-user.

The product side is `RUN_PROTOCOL` rules 9–12 in every `PROMPT.md`, with the field-key
classification `FIELD_ACTION` in `concepts::build_plan::plan`.

## Hard rules

- Never `--no-verify`.
- Never set `PARSER_CORPUS_BLESS` to get green.
- Never raise a floor or a cap mid-gate.
- Never judge by piped output instead of the exit code (CORE-1).
- Never commit around a red gate or report success past one.

## Encode the class

An escaped mistake becomes, in order of preference: a new invariant rule plus its `SEEDS` row, a
new test, or a `[dev]` checklist line below plus its mirrors. A mistake class is not recorded as a
memory file; memory keeps preferences and context.

## Checklist

`[dev]` phrases are mirrored verbatim in `.claude/skills/attack/SKILL.md` step 6 and
`.claude/rules/core.md` (checked by `scripts/skill-check.sh`, the `checklist-mirror` rule);
`[product]` phrases appear verbatim in `RUN_PROTOCOL` (checked by `plan::tests`).

1. **Re-run the whole gate from step 1 after any fix** [dev]
2. **Judge every step by its exit code, never by piped output** [dev]
3. **Act on a finding by what fixing it would change: no-op, auto-fix or ask-user** [dev]
4. **Never weaken a check to go green** [dev]
5. **Declare every fixture, snapshot or floor change under ## Expectation changes** [dev]
6. **Encode an escaped mistake as a rule, a test or a checklist line, never a memory** [dev]
7. **Act on a failure by what fixing it would change, not by how bad it looks** [product]
8. **Never make a check pass by weakening it** [product]
9. **then the full gate from its first step** [product]
10. **A mistake that escaped a task is encoded, not remembered** [product]
