# ADR-0030 — A gated build plan replaces the raw build prompt

- **Status:** Proposed
- **Date:** 2026-09-29
- **Deciders:** Owner
- **Amends:**
  - [ADR-0022](./0022-skills-as-markdown-and-the-skill-book.md): the capstone's output becomes a
    gated build-plan artifact.
  - [ADR-0023](./0023-verification-layer.md): §1, fenced persistence remains only for owner skills
    that declare `fenced_markdown`. §3, the evidence gate moves to `domain::evidence` and folds
    backslashes.
- **Extends:**
  - [ADR-0015](./0015-knowledge-extraction-artifacts.md): a new artifact kind.
  - [ADR-0021](./0021-reference-sources.md): attached sources back deterministic anchor checks.

## Context

The two capstone chips on the idea page turn a discussion into a prompt for a coding agent:
- the quick `build-prompt` skill;
- the dashed `ready-to-build` workflow, which harvests, then audits, then chains `build-prompt`.

We reviewed every build prompt in the vault (6) and checked one of them line by line against the code.
The same defects recur:

- Open questions are pinned as "settled" (5 of 6 prompts). None of the prompts has an open-questions
  section.
- Gates the discussion demanded ("must not be built without …") are dropped or contradicted.
- `file:line` anchors are cited against the wrong lines, and syntax that exists nowhere is presented as
  a fact (`[[slug#fact]]`).
- Figures in different units are combined ("6 facts" becomes "the remaining 30 pairs").
- The plan fences a module off and then orders edits inside it. Streams described as parallel actually
  depend on each other.
- Acceptance criteria are written as prose, not runnable commands.
- In the workflow, REFUTED findings reach the build step with no instruction to set them aside.
  `docs/06-concepts/workflows.md` says "surviving findings" while the code passes all of them.

7 of the 12 errors in the checked prompt were already in the discussion, so the output copied them
faithfully. A gate that only asks "is this verbatim in the discussion?" catches none of those 7. What
catches them is structure plus cheap deterministic checks:
- an anchor must name the symbol it points at;
- a code token must exist somewhere;
- a number must carry its unit or its count command;
- a premise must carry the check that would settle it;
- a fence must be compared with each task's `touches`.

The owner's harness has a "librarian" (backend-mono `/work-hard`) that keeps evidence-gated recipes
for coding agents. Its machinery (a recipe book, routing indexes, a curating agent) has no corpus to
serve here. Its rules do transfer.

## Decision

1. **The output is a build plan, not a fenced blob.** The model writes these sections:
   `## Goal`, `## Settled` (one `S#` per item, each with a `quote:`), `## Verify first` (`P#` with
   `check:`), `## Open questions` (`Q#`), `## Plan` (`T#` with `depends:`, `touches:`, `accept:`) and
   `## Kill criteria` (`K#` with `checked by:` and `gates:`), plus an optional `## Fence`. A new
   `OutputContract::BuildPlan` repairs near-misses and reuses the one existing retry.
2. **Deterministic gates G1–G14 run after the model call.** They make no model call, hold no semaphore
   permit, and never execute a command. `gates::run` applies them in order: claims (G1–G3), sources
   (G4–G6, G12), tasks (G7–G11), the leaf gate (G13), then the task-graph lint (G14); see Amendments.
   - Quotes are grounded, and provenance is recorded as owner, foil or idea.
   - An item that collides with an open question or an UNCERTAIN finding moves to Open.
   - A REFUTED finding moves to Quarantined, with the auditor's reason.
   - Anchors are paired with a symbol and resolved against attached sources, else marked unverified.
   - Code tokens are checked against sources, then the owner's words, then the foil's.
   - Figures and units, the scope fence, dependency repair, runnable accept commands, kill-criteria
     wiring, caps (split, never trim) and freshness are each checked. An accept that is a known
     runner command with a `→` condition but no backticks is repaired (`⟨accept repaired⟩`) rather
     than demoting its task.
   - Every demotion names the check that would re-promote it.
3. **The plan is an artifact.** It is written to `artifacts/<stamp>-build-plan.md` (`kind: build_plan`).
   The transcript gets only a pointer turn: the link, the mode label, the gate tally and the open
   questions. The plan body never re-enters the discussion, so it cannot become its own evidence and
   does not crowd a small model's context.
4. **Two depths, same routes and names.** `⌁ quick build prompt` posts the existing `build-prompt`
   skill route: one planner call (plus at most one reshape retry), then the gates.
   `⌁⌁ audited build plan` posts the existing `ready-to-build` workflow route: five harvesters, one
   audit, one planner call, then the gates. The two chips sit side by side in the capstone row.
   Registry names, turn headings and the coverage spine are unchanged. Both persist boundaries
   (`skills::invoke` and `run_workflow`) route through one `build_plan::finish`.
5. **Copy-ready projections are derived at view time.** The artifact page's "Use it" box offers a
   `PROMPT.md` and a `plan.md` (for `/attack --loop`). Neither is stored, and both are escaped.
   - `PROMPT.md` is a run protocol. It splits PINNED (the owner's own words) from Foil conclusions,
     which are headed "confirm at bootstrap" only when an audit stood behind them and "unverified,
     confirm before building" otherwise.
   - In `plan.md`, `wave`, `score` and `model` are derived by the gates, never read from the model.
6. **The workflow's findings block** carries the auditor's reason and the swarm's CONFIRMED / UNCERTAIN /
   REFUTED guidance, is unlabelled when the audit failed, and is clipped. An empty harvest is an error
   (`NothingHarvested`, "harvest produced nothing; use the quick build prompt"), and nothing is
   persisted.

### What the librarian contributes: rules, not machinery

| Librarian rule | Here |
|---|---|
| Never promote on one gate | Settled requires a grounded quote and no collision with the audit or with open questions |
| Only the human's words count as approval | PINNED versus Foil-conclusions provenance |
| Cite an anchor only after opening it | ✓ only when the probe opened the file; otherwise "unverified", with the command |
| An unrecorded miss looks like never having looked | A header tally and a "consulted" line record misses |
| The remedy for a cap is to split, never to trim | G11 reports an over-cap plan and drops nothing; G13 asks for a split instead of trimming a task |

**Not ported:**
- an `AgentRole::Librarian`;
- a model-driven context bundler (`ai::budget` and ADR-0027's related-ideas block already cover this);
- a recipe book, router indexes, curate jobs;
- promotion of findings into `vault/.skills`.

**Re-entry criterion for promotion:** at least 3 ideas carry build-plan artifacts, and a CONFIRMED
finding recurs (near-duplicate) in at least 2 of them.

## Amendments (2026-09-29)

The plan now targets a coding agent that runs it as a set of leaf tasks, in waves. The six-section
shape and the persist boundary are unchanged; the grammar, two gates and both projections grow.

### Grammar

- A `## Plan` task may carry `red:` (a command that must fail before the edit), `reads:` (paths to
  open first), `stop if:` and `exempt:` besides `depends:`, `touches:` and `accept:`.
- Inside `## Plan` only, line-form aliases map to the canonical keys: `files`/`file`/`paths` →
  `touches`; `depends on`/`after`/`blocked by` → `depends`; `red-first`/`fails before` → `red`;
  `open first`/`context`/`inputs` → `reads`; `test`/`command`/`acceptance` → `accept`.
- `depends:` may cite `P#` and `Q#` as well as `T#`. A `Q#` holds the task as `[?]` (`blocked by
  Q#`, G8); a `P#` makes the task wait for that premise's bootstrap check.
- `score`, `model` and `wave` are derived by G14; `leaf` and `was` are reserved for the code
  (`was` records a quarantined item's original id). `plan::parse` drops all five from a model
  answer, so a model cannot pre-write a wave or a gate pass; `plan::parse_artifact` reads them back
  from a stored artifact, with every `⟨…⟩` marker.
- `T0` is reserved for the code-owned bootstrap row. A model-written `T0` is renumbered to the next
  free `T#`, with the task and kill references to it, in both `parse` and `parse_artifact`.

### G13 — leaf gate (`gates/leaf.rs`)

Each task is checked against the leaf invariants. Every marker starts with `leaf: `, so no other
gate's marker reads as a leaf finding.
- a subject naming two commits → `leaf: split: one commit`;
- `touches` under more than one top-level root → `leaf: crosses roots`;
- a compound accept → `leaf: compound accept`; a test runner with no passed count →
  `leaf: no count: a filter matching 0 tests exits 0`;
- more than 3 non-test files, or more than 6 `reads` plus `touches`: one trigger gives a
  `leaf: justify:` note unless the task carries `exempt:`, two give `leaf: split:`;
- a behaviour verb with no `red:` → `leaf: no red-first proof`; a sweep whose accept is not a grep
  → `leaf: sweep: end with a grep printing 0`.

A one-task, one-file plan gets no split notes. Empty `touches` is tallied as `unscoped`. G13 adds
markers and tally counts only and never demotes a task to `[?]`, so a weak model's plan stays
buildable and the executor decides whether to split.

### G14 — task-graph lint and derivation (`gates/tree.rs`)

The task graph is checked as a whole:
- duplicate ids are renumbered, and dangling `T#`/`P#`/`Q#` references and self-edges are dropped
  with a note;
- **premise wiring:** a Verify-first `P#` joins the `depends` of every task whose `touches` or
  backticked text names one of the tokens in the premise's backticked spans (`premise P# wired`),
  or that shares a record id with the premise anywhere in its text (`ADR-002`, `JIRA-1234`: two or
  more uppercase letters, a hyphen, two or more digits; one-digit names such as `UTF-8` and
  standard prefixes such as `SHA-256`, `ISO-4217` or `RFC-9110` do not count). Other
  plain words outside backticks never wire. A one-word span wires only when it is path-like or
  identifier-shaped; a directory-level path never wires by overlap; decimals, versions and
  unit-suffixed numbers (`0.7`, `v1.2`, `1.5x`) are not paths;
- every kill row must gate a real task (`gates no task`);
- cycles are named and their tasks go `[?]`;
- a task citing a Quarantined claim, by its id or by the id it had before quarantine (the recorded
  `was`), goes `[?]`;
- a task depending on a `[?]` task inherits `[?]` (`blocked by T#`).

Each task then gets code-owned fields:
- `score`: DBVKC digits. D = an open `Q#`, a refuted upstream or a quarantined dependency;
  B = empty `touches`, more than one non-test file or more than one root; V = no single runnable
  accept with a count; K = a gate-surface path (migrations, schema, lockfiles, auth, env,
  generated, container and manifest files) or a Fence path; C = `reads` plus `touches` over 6.
  A score of 4 or more is marked `split before building`.
- `model`: `sonnet` for a score of 0–1, `opus` for 2–5, plus `review@opus` when K is set.
- `wave`: topological layers over the ready tasks. A task whose `touches` overlap a wave member's
  moves to the next wave; a wave holds 3 tasks, or 2 once a member is on the gate surface; an
  unscoped task sits alone in its wave. `[?]` and cyclic tasks get no wave.

Premise edges are advisory for waves, which follow only `T#` edges. `plan.md` orders them through
its `T0` row.

The design study numbered premise wiring as a separate G15. The code runs it inside G14, and the
gate count stays at G1–G14.

### `PROMPT.md` — a run protocol

In order:
1. the goal's first line as the title, the idea and plan stem, and a `_what ran: <mode> · gates:
   <tally>_` line; the rest of a multi-line goal follows as one quoted paragraph;
2. a trust line: the mode, the audit tally or `unaudited`, when and by which model, `sources:
   attached` or `no sources: anchors unverified`, and the capstone turns kept out of evidence. A
   header that never recorded a segment (a legacy artifact) reads `not recorded` for it;
3. a fixed, code-owned `## How to run this` in 12 steps. Step 1 creates one task-tracker entry per
   row, plus T0 when there are Bootstrap checks, grouped by wave. Step 7 sets the parallel-wave
   rules: T0 first and read-only, same-wave tasks in isolated worktrees or subagents, integration at
   each wave boundary by merging or cherry-picking each task's commit, then every finished accept
   and the full gate re-run before the next wave. Step 11 forbids destructive commands and history rewrites;
4. a one-line `Waves:` summary (`unscheduled` for tasks with no wave);
5. PINNED, Foil conclusions (labelled unverified when the mode is quick, unaudited, or the audit
   failed, was skipped or is not recorded), Fence, Bootstrap checks and Ask the owner;
6. each task as a leaf brief: objective, files, open first, depends (each premise's check inlined),
   acceptance, red-first, stop if, wave/score/model and every gate marker;
7. each kill row as a `STOP if …; checked by T#; blocks T#` line, and Do not build on.

### `plan.md` — the `/attack --loop` table

- Header lines: `Goal:`, `Rules: PROMPT.md (How to run this, PINNED, Fence)`, `Selection rule:`
  (the topmost `[ ]` whose Depends are all `[x]`, never `[?]`), `Commands:` (a `\|` in a cell is
  the markdown table escape for `|`; run the command with a plain `|`), `Fence:` and one STOP line
  per kill row.
- Columns: `[ ] | T | Task | Depends | wave | score | model | touches | accept`. On task rows,
  `wave`, `score` and `model` come from G14, never from the model.
- Only when Verify-first premises exist, a `T0` bootstrap row comes first. Its `0 | 00000 | haiku`
  cells are fixed by the projection, not derived by G14, and its accept joins every `P#` check. The
  row renders `[ ]`; its Task cell instructs the executor to mark T0 `[x]` once every check has run,
  log each failed `P#`, and mark `[?]` every task whose premises list it. Every task relying on a
  premise depends on `T0` and names its premises in the Task cell (`(premises: P#…)`).
- A premise without a check holds only the tasks citing it: they are `[?]`, with no wave, until the
  owner confirms it by hand.
- A `[?]` row carries its reason in the Task cell. An empty `## Log` closes the table.

### Workflow side

- The planner in `ready-to-build` gets a code-owned "How to use the findings" preamble that routes
  each finding by its kind label (decision, open question, risk, next action, fact) and verdict.
  Preamble and findings together take at most a third of the budget, above a small floor for the
  findings block.
- The mode label names what ran: `quick · unaudited`, `audited`, `audited · uniform pass (weak)`,
  `ready-to-build · audit failed` or `ready-to-build · audit skipped (<reason>)`.
- An empty harvest errors with `NothingHarvested`, and an answer with neither a goal nor a task
  errors with `PlanUnusable`. Neither persists anything. `WebError` maps both to HTTP 422, which
  applies only to a synchronous caller; the chips run background jobs, so the owner sees the
  message as the job's error on the next `/pending` poll.
- Non-plan workflows and swarms end with code-owned lines: `_N further findings left out (cap 20)_`
  (or `not audited`, when an audit ran) for findings past the audit cap, and
  `_k of N angles answered; missing: …_` when an angle returned nothing.

### After the first live run (2026-09-29)

Both chips were run against a copy of the vault with the claude-code backend on an idea with an
attached source. Four defects were fixed:
- **A fence is never removed.** G5 treats a Fence item as a guard, not a claim: an unproven path
  stays fenced with an `unverified fence path: …` marker instead of moving to Quarantined, which
  had left `plan.md` with `Fence: none` over the owner's read-only reference docs.
- **An absolute source path resolves.** On claude-code the model sees a source by its absolute
  root, so `SourceProbe::has_path` and `check_anchor` read a path at or under a root
  root-relative, at that exact place in that root only (no suffix match, no other root). That
  path had read as absent.
- **A kill row needs no stop word.** G10 no longer requires "stop"/"kill"/"halt" in the row text:
  every row is projected as `STOP if …`, and 5 of 6 live rows were flagged for lacking the word.
  `checked by`, `gates` and the continue-anyway check remain.
- **Tasks cite the premises they rely on.** The template's `depends:` line asks for `P#`, and
  premise wiring also matches record ids (above). In the live run no task depended on any premise,
  so `T0` gated nothing.

The `plan.md` header also gained the `Commands:` line: copied literally, `grep -cE "A\|B"` counts
0 where `grep -cE "A|B"` counts 2.

## Known misses

These are named here rather than in each plan:
- prose contradictions not written as K rows;
- a benchmark claim transferred beyond its scope;
- lowercase invented names that are not backticked;
- a Settled claim that says more than its quote: G1 checks that the quote is the owner's verbatim
  words, not that the claim follows from them. Live, a quoted owner question carried the foil's
  answer into PINNED;
- an UNCERTAIN harvested finding that restates the owner's own words moves them to Open, so the
  audit's doubt can outrank the owner's statement;
- whether hydration clipped the discussion: only the audited (`ready-to-build`) planner is told,
  the quick path is not, and the artifact header records it for neither, so the trust line reads
  `truncation not recorded`.

## Alternatives rejected

- **A model judge as a gate.** It isn't deterministic and costs a call on a small local model.
- **Executing check commands.** The app must never run arbitrary commands.
- **Linting the old fenced blob after the fact.** It has no structure to gate.
- **A Librarian role.** The role enum is closed. The job is routing, not a persona.
- **A new `/build` route.** Both depths fit the existing routes.
- **Keeping the plan inline in the transcript.** That lets the plan ground itself and bloats the
  context.
- **A frontmatter stamp.** It forces an index edit for no reader.
