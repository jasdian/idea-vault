# ADR-0034 — Grounded, ranked and bounded workflow stages

- **Status:** Accepted — amended by [ADR-0037](./0037-run-journal-diagnostics-only-call-record.md) (the call budget is charged by billed requests, not by steps)
- **Date:** 2026-09-30
- **Deciders:** Owner
- **Amends:** [ADR-0006](./0006-bounded-concurrency-swarm.md) (a workflow's total model calls are
  capped by an exact ceiling, and stage calls each take one permit),
  [ADR-0021](./0021-reference-sources.md) (attached sources are also read by the Ground stage, on a
  narrowed tool budget), [ADR-0023](./0023-verification-layer.md) (Panel scorers are cold Auditor
  calls; Refine re-audits; a Panel's proposals and a Loop's items are audited like any findings).
  Makes one scoped exception to the discard-intermediates rule of [D14](../06-concepts/swarm.md).
- **Extends:** [ADR-0022](./0022-skills-as-markdown-and-the-skill-book.md),
  [ADR-0030](./0030-gated-build-plan.md).

## Context

A workflow ([D32](../06-concepts/workflows.md)) had four stage kinds: fan-out, chain, audit and
synthesize. Four gaps kept it from doing what the product promises, "run it into the ground":

1. **No grounding.** An idea that changes code is argued about without checking that the files and
   symbols it names exist. The build-plan gates ([ADR-0030](./0030-gated-build-plan.md), G4) verify
   anchors only after a plan is written.
2. **No comparison.** Competing designs are merged and synthesized, never ranked; whichever the
   synthesizer likes wins.
3. **No stop rule.** One pass of critics either finds enough or it does not; there is no "again until
   nothing new".
4. **No repair.** The audit labels findings REFUTED or UNCERTAIN and the run moves on; nothing
   reworks them.

Facts checked in the code before this decision: every model call goes through `run_agent` or
`ask_on_contract` and takes exactly one permit; `swarm::judge` is a deterministic dedupe, so the name
"judge" is taken; `AgentRole` has six roles and `SkillRole` five (no Auditor); the `SourceProbe`
already resolves anchors (`check_anchor`, `find_tokens`) for G4; there is no `regex` dependency; and
a workflow run persists one turn (or a gated plan plus its pointer turn) and nothing else, so a
workflow could not keep the evidence of what a stage did.

The target is a small local model on CPU. Every stage is therefore judged on: does code, not the
model, decide control flow, termination, dedup, totals, winner and caps; does a weak model's failure
degrade rather than abort; is the worst-case cost known before the run.

## Decision

We will add four stage kinds, decided in code, and give every workflow an exact call ceiling.

1. **Ground** (`readers` 0–3 default 2, `tool_rounds` 1–2 default 2, optional one `angles` line per
   reader). With no source attached it is skipped, no call, nothing carried, no artifact. Otherwise a
   code map (an outline, plus path-like and backticked tokens mined by hand from the idea and the last
   six turns, each resolved against the files), then up to three readers (the hidden `ground-read`
   skill, role Researcher, on a 2 × 2 tool budget through `LlmBackend::with_tool_budget`), each
   answering at most eight `` - `path:N[-M]` | `symbol` | claim `` lines. Code then normalises and
   dedupes the claims and checks every anchor with `SourceProbe::check_anchor`: Resolved is verified,
   Moved is verified and re-anchored, NoFile / SymbolMissing / Ambiguous is disproved, Unverified
   (a capped walk) stays unverified and is never called disproved. Only verified anchors, the outline,
   the paths that do not exist and an unverified count are carried, as
   `## Prior stage: grounded map (anchors verified, claims not)`, capped at a quarter of the stage
   budget; a disproved claim's text is never carried. It verifies existence, not meaning.
2. **Panel** (`proposers` 2–4, `criteria` 2–5 each with a slug name, weight 1–3 and 0/2 anchors,
   `judges` 1–2). Proposals are fanned out under the `Proposal` contract. If fewer than two survive it
   is "no contest": no scorer is called and the survivors become plain findings. Otherwise each
   proposal is scored **alone**, cold, by the hidden `panel-score` skill run as
   `AgentRole::Auditor`, seeing that one proposal, the idea and the rubric, never another proposal and
   never the related-ideas block; a missing or garbled line scores 1 and is flagged; with two judges
   each cell is the median, the lower value on a split. A pure `panel::aggregate` computes weighted
   totals, breaks ties (fewer zeros, then the higher score on the first top-weight criterion, then the
   lower index) and lists grafts for each criterion a runner-up wins. The Synthesize stage that
   follows runs in graft mode (the winner as the spine, only the listed grafts), and code strips any
   `Grafted from P<k>` line naming a proposal that does not exist or is the winner.
3. **Loop** (`steps` 1–4, `dry_rounds` 1–2, `max_rounds` 2–4, `max_calls` at most 16). Before each
   round code checks the calls, the rounds and the run's call budget; from round two each step is told
   what is already found. Novelty is `near_duplicate`, judged in step order. A round in which every
   agent failed spends its calls and resets nothing. Stop reasons are Dry, Cap and Failed.
4. **Refine** (`role`, `skill`, `max_rounds` 1–2), valid only right after an Audit. It is skipped with
   no call when the audit is off, failed, or has no REFUTED or UNCERTAIN verdict; otherwise a round
   is one rewrite call over those findings and one full re-audit, the replacements applied **by id in
   code**.
5. **Call ceiling.** `Workflow::call_ceiling` is the exact worst case, repair retries included (fan-out
   n, chain 2, audit 1, synthesize 1, Ground readers × 2, Panel n + judges × n, Loop
   min(`max_calls`, `max_rounds` × steps), Refine 2 × `max_rounds`). A definition over
   `WORKFLOW_MAX_CALLS` (32) is rejected at load, never clamped. At run time a `CallBudget` reserves
   the ceiling of every later stage, so an elastic stage cannot starve the final one. The ceiling and
   the waves (`⌈widest stage / K⌉`, ADR-0006) are shown on the chips and the skill book before a run.
6. **Stage artifacts, a scoped exception to D14.** Ground, Panel and Loop each stage at most one
   artifact (kinds `ground_map`, `scorecard`, and a `finding` with lens `loop`), plus one
   `workflow_run` record with a row per stage (kind, status ran / skipped / degraded with reason,
   calls, detail). They are written **only after the final persist succeeds**, in the existing
   await-free tail, with slugs `<run-stamp>-<workflow>-<n>-<stage>` and `<run-stamp>-<workflow>-run`,
   and named on the turn's trailing `Stage artifacts: [[…]] · …` line. A capstone's pointer turn is
   not modified (its prefix must stay first), so the run record names the plan instead. A cancel or a
   failed final stage persists nothing. They are never turns and never memory evidence, and reindex
   indexes them through the generic artifact walk.
7. **Naming.** No `AgentRole::Judge` and no rename of `swarm::judge`; the Panel's scorers are the
   Auditor role running a hidden skill. `panel-score` is `role: critic` in its frontmatter because
   `SkillRole` has no Auditor; the Panel stage overrides the call role in code and a test pins it.
8. **Progress** stays the single `jobs::set_note` string: `workflow · {name} · {i}/{n} {kind}:
   {detail} · calls {used}/{ceiling}`.

The built-ins are `design-panel` (Ground → Panel → Audit → Synthesize; worst case 12, 8 without
sources), `exhaust` (Loop → Audit → Refine → Synthesize; worst case 13) and `ready-to-build`, which
gains a leading Ground (worst case 12 with sources, 8 without, unchanged without sources).

### As built, where it differs from the proposal

- `exhaust`'s ceiling is 13, not 16: a Loop's ceiling is min(`max_calls`, `max_rounds` × steps), which
  for 12 calls and 3 rounds of 3 steps is 9.
- A skipped Ground carries no `## Prior stage` block at all, rather than a "grounding skipped" line,
  so a workflow over an idea with no sources builds byte-identical prompts to before Ground existed.
- A Panel proposal is shaped by `contract::validate` with a fallback to the trimmed text, not repaired
  by a second call, and a scorer call is not repaired either (a failed scorer leaves its cells
  unscored); this is why the Panel's ceiling has no repair term.
- A Loop whose first round fails outright is a degraded stage with reason Failed rather than an error;
  the failure surfaces at the next stage that needs findings.

## Consequences

- Easier: an idea about code is checked against the code before it is argued; competing designs are
  ranked by arithmetic the owner can read; "keep attacking until nothing new" and "fix what the audit
  rejected" are one line of frontmatter; the cost of a run is known before it starts; a small model's
  garbled scorecard degrades to a flagged cell, not a wrong winner.
- Harder: a workflow can now cost up to 32 model calls, and `design-panel` can take ten minutes or
  more on a CPU-only Ollama at K=2, which is why the ceiling is shown first. The vault gains three
  artifact kinds, each of which the artifact page and the reindex must tolerate.
- Invariants: model calls always take exactly one permit and the runner holds none; stage artifacts are
  written only after the final persist and never as turns or evidence; every new `Artifact` literal
  sets `revises`, `version` and `answered` to their empty values; a Ground miss is disproved only when
  the probe's walk was complete.
- Known limits. Ground verifies existence, not meaning; the audit stays the check on meaning. On
  Ollama, `num_ctx` is not re-floored after tool-result growth (ADR-0021); the 2 × 2 reader budget
  reduces the risk without removing it. claude-code readers run the CLI's own agent loop, which
  `with_tool_budget` cannot bound and which can read outside the attached sources; such a claim
  resolves under no source root and is disproved, never carried. A weak model may leave a panel
  unscored; the scorecard shows it. Running code stays out of scope: plans hand off to a coding
  agent.

## Alternatives considered

- **An engine-first design with a new `AgentRole::Judge`** (renaming `swarm::judge`): rejected. It
  touches D14 and four ADRs for a name.
- **An owner-first design that persists an "incomplete" record when the final stage fails**: rejected.
  It breaks the all-or-nothing rule that a failed final stage leaves the vault untouched.
- **A new jobs progress API** (a structured ribbon): rejected. The single note string renders
  verbatim, so there is no template change.
- **Scorers that see every proposal**: rejected. Position bias would need rotation bookkeeping; one
  proposal per scorer removes it for free and keeps each scorer's context small.
- **A model that picks the winner or writes the graft ids**: rejected. Code owns totals, tie-breaks,
  grafts and the id check.
- **Letting Ground carry reader claims unverified**: rejected. The point of the stage is that
  what reaches later stages exists.
- **A `regex` dependency for token mining**: rejected. The scan is hand-written; the crate list
  does not grow.

## Amendment — ADR-0037 (2026-09-30)

> **Amended by [ADR-0037](./0037-run-journal-diagnostics-only-call-record.md).** The `CallBudget` is
> charged as requests go out, by a meter on the run's backend view, not by counting steps: a retry is
> one more, each Ollama tool round is one more, a claude process is one. The exact ceiling per stage
> is unchanged and still shown before a run; a tool-using call can now cost more than the one call its
> stage's ceiling assumed, so the reserve check funds fewer elastic rounds. The audit's targeted re-ask
> ([ADR-0040](./0040-recipe-provenance-and-audit-re-ask.md)) is charged like any other request and is
> attempted only when the budget can fund two calls.

---

> ADRs are immutable once **Accepted**. To change a decision, write a new ADR that supersedes this
> one and update the Status line above.
