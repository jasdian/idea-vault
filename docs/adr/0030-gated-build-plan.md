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
   `## Kill criteria` (`K#` with `checked by:` and `gates:`). A new `OutputContract::BuildPlan`
   repairs near-misses and reuses the one existing retry.
2. **Deterministic gates G1–G12 run after the model call.** They make no model call, hold no semaphore
   permit, and never execute a command.
   - Quotes are grounded, and provenance is recorded as owner, foil or idea.
   - An item that collides with an open question or an UNCERTAIN finding moves to Open.
   - A REFUTED finding moves to Quarantined, with the auditor's reason.
   - Anchors are paired with a symbol and resolved against attached sources, else marked unverified.
   - Code tokens are checked against sources, then the owner's words, then the foil's.
   - Figures and units, the scope fence, dependency repair, runnable accept commands, kill-criteria
     wiring, caps (split, never trim) and freshness are each checked.
   - Every demotion names the check that would re-promote it.
3. **The plan is an artifact.** It is written to `artifacts/<stamp>-build-plan.md` (`kind: build_plan`).
   The transcript gets only a pointer turn: the link, the gate tally and the open questions. The plan
   body never re-enters the discussion, so it cannot become its own evidence and does not crowd a small
   model's context.
4. **Two depths, same routes and names.** `⌁ build plan` posts the existing `build-prompt` skill route.
   `⌁ build plan · audited` posts the existing `ready-to-build` workflow route. Registry names, turn
   headings and the coverage spine are unchanged. Both persist boundaries (`skills::invoke` and
   `run_workflow`) route through one `build_plan::finish`.
5. **Copy-ready projections are derived at view time.** The artifact page offers a `PROMPT.md` and an
   `@plan.md` (for `/attack --loop`). Neither is stored, and both are escaped.
   - `PROMPT.md` splits PINNED (the owner's own words) from Foil conclusions (confirm at bootstrap).
   - In `@plan.md`, `score` and `model` are left as `?`.
6. **The workflow's findings block** carries the auditor's reason and the swarm's CONFIRMED / UNCERTAIN /
   REFUTED guidance, is unlabelled when the audit failed, and is clipped. An empty harvest skips the
   audit with a note.

### What the librarian contributes: rules, not machinery

| Librarian rule | Here |
|---|---|
| Never promote on one gate | Settled requires a grounded quote and no collision with the audit or with open questions |
| Only the human's words count as approval | PINNED versus Foil-conclusions provenance |
| Cite an anchor only after opening it | ✓ only when the probe opened the file; otherwise "unverified", with the command |
| An unrecorded miss looks like never having looked | A header tally and a "consulted" line record misses |
| The remedy for a cap is to split, never to trim | G11 reports an over-cap plan and drops nothing |

**Not ported:**
- an `AgentRole::Librarian`;
- a model-driven context bundler (`ai::budget` and ADR-0027's related-ideas block already cover this);
- a recipe book, router indexes, curate jobs;
- promotion of findings into `vault/.skills`.

**Re-entry criterion for promotion:** at least 3 ideas carry build-plan artifacts, and a CONFIRMED
finding recurs (near-duplicate) in at least 2 of them.

## Known misses

These are named here rather than in each plan:
- prose contradictions not written as K rows;
- a benchmark claim transferred beyond its scope;
- lowercase invented names that are not backticked.

## Alternatives rejected

- **A model judge as a gate.** It isn't deterministic and costs a call on a small local model.
- **Executing check commands.** The app must never run arbitrary commands.
- **Linting the old fenced blob after the fact.** It has no structure to gate.
- **A Librarian role.** The role enum is closed. The job is routing, not a persona.
- **A new `/build` route.** Both depths fit the existing routes.
- **Keeping the plan inline in the transcript.** That lets the plan ground itself and bloats the
  context.
- **A frontmatter stamp.** It forces an index edit for no reader.
