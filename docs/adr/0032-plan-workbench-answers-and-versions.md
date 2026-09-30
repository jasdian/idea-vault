# ADR-0032 — Plan workbench: owner answers make a new plan version

- **Status:** Accepted
- **Date:** 2026-09-30
- **Deciders:** Owner
- **Amends:** [ADR-0030](./0030-gated-build-plan.md) — a plan gains a lineage and a workbench route
  group; two of its rejected alternatives are reversed (below).
- **Extends:** [ADR-0023](./0023-verification-layer.md) (an owner answer is Owner evidence);
  [ADR-0022](./0022-skills-as-markdown-and-the-skill-book.md) (a pointer turn counts in skill
  coverage).

## Context

A build plan ([ADR-0030](./0030-gated-build-plan.md)) ends with open questions. The old pointer
told the owner to "answer in chat, then build again". That produced the owner's complaint: the
new plan was a fresh, unrelated model run, so it renumbered ids, re-asked the answered questions
and dropped the answers. Nothing linked one plan to the next, and a targeted fix ("answer Q6")
had to go through a model call even though the answer only needs to be recorded and the gates
re-run.

Constraints that bear on the design:
- Markdown is truth ([ADR-0002](./0002-markdown-source-of-truth-sqlite-index.md)); an artifact is
  never edited in place once written by a run.
- The evidence gate credits a turn by its heading: only the exact role `user` is Owner
  (`parse_turn_heading`, `src/vault/store.rs`); any other `user (...)` heading falls into
  `TurnSource::Other`, which `Evidence::new` credits as Foil.
- The code must never copy question text into an owner turn
  ([ADR-0023](./0023-verification-layer.md)): the owner's words are the evidence.
- A model may write `[?]` on a task, and the gates derive `[?]` too; a re-gate has to tell them
  apart.
- AI turns are background jobs ([ADR-0010](./0010-ai-turns-as-background-jobs.md)); an answer
  makes no model call, so it must not take a job slot.

## Decision

We will add a **plan workbench** on the existing artifact page (R19): the owner answers open
questions (`Q#`) and owner-held tasks (`T#`) in their own words, and each submission makes a new
plan **version** deterministically. Six rules:

1. **An answer is a plain owner turn.** Each answer appends one `## user` turn with the body
   `Re Q6 (<base-stem>): <owner's words>`. The prefix contains no word from `OPEN_MARKERS` or
   `HEDGES`, the code never copies the question into it, it is not a capstone turn (so it is Owner
   evidence) and it triggers no foil reply.
2. **Answering is deterministic and synchronous.** `workbench::answer` makes no model call, takes
   no job slot and holds no semaphore permit, like rename (R23) and tags (R42). It is refused
   while a job runs for the idea (`jobs::is_running`, non-consuming), and a process-wide
   `WORKBENCH_LOCK` serialises the head check with the writes.
3. **A version is a new artifact; the base is never modified.** The new
   `<stamp>-build-plan.md` carries `revises: <base>`, `version: n+1` and `answered: [Q6, T4]`.
   The lineage is linear and answers are accepted only on the head; an older version answers with
   `Superseded { head }`. A plan without lineage fields reads as a version-1 root.
4. **Every plan run joins the lineage.** `finish_as` (quick, audited, web or MCP) sets `revises`
   to the current head, carries every owner answer on the head's chain into the new plan
   (`carry_answers`) and drops an Open question that re-asks one (`suppress_answered`, each drop
   noted in the gate report). There is no "fresh thread" option. For the capstones only, the
   prompt gains a `## Prior plan (ids only — not evidence)` block (the head's open ids and
   texts, each `answered Qn → words`), capped at 1500 bytes: it keeps ids stable and is never
   evidence.
5. **A deterministic version re-gates the plan.** `parse_artifact`, then `reset_derived`, then
   `apply_answers`, then `gates::run` with `audit: None`, against fresh on-disk evidence. The
   header reads `v{n} · answers on <base> · audit not re-run`. The plan grammar gains owner-only
   item fields (`answers`, `asked`, `in`, `unblocks`, `unblocked`) which the untrusted model
   parse drops, so a model cannot forge an answer, and an `owner: model` field on a task the
   model itself marked `[?]`, so a re-gate keeps the model's box and re-derives the gates'.
6. **An owner answer outranks a doubt.** A Settled item with an `answers` field and Owner
   provenance is exempt from three G2 signals: model-open collision, audit-open collision and
   open-questions-artifact listing. `hedge` and `marker_beside` still apply, because a hedge in
   the owner's own words means the question is still open (the item goes back to Open, keeping
   `answers`, and the workbench shows an inline warning).

Routes: R46 `POST /idea/{slug}/plan/{stem}/answer`, R47 `GET /idea/{slug}/plan/latest`, R48
`POST /idea/{slug}/plan/{stem}/replan` (the model re-plan, a background job). Diagram: D33.

**Reverses two [ADR-0030](./0030-gated-build-plan.md) alternatives.**
- *"A new `/build` route"* — R46-R48 are new routes. The reason: the workbench needs a write
  path that is not a model call, and re-plan needs the lineage head as its target; neither fits
  R6/R22.
- *"A frontmatter stamp"* — `revises`, `version` and `answered` are now stamped. The reason: the
  lineage is a graph over artifacts and only frontmatter can carry it without a second store. The
  SQLite index is untouched (frontmatter is read from disk; `reindex` still rebuilds everything).

## Consequences

- **Answer turns feed chat context and store-time extraction.** They are ordinary `## user`
  turns, so the foil sees them and `store` may extract memory from them, subject to the verbatim
  quote rule of [ADR-0023](./0023-verification-layer.md).
- **The audit is not re-run on an answered version**, and the audit view is not persisted, so a
  version's header says `audit not re-run`. Follow-up: persist the `AuditView` with the artifact
  so a version can re-gate against it.
- **`[?]` ownership is explicit.** A task's `[?]` survives `reset_derived` only where the model
  wrote it (`owner: model`) and never once the owner answered it (`unblocked`). A plan from before
  the field existed re-derives a `[?]` that no marker explains as model-owned.
- **A T# is answerable only when its sole hold is the model's own box or the marker
  `needs you`.** A Q-block, a cycle, `no runnable accept`, a destructive command or a fenced path
  is structural: the workbench shows the reason and offers a re-plan instead.
- **A pointer under a skill heading counts in skill coverage** ([ADR-0022](./0022-skills-as-markdown-and-the-skill-book.md)):
  the answer pointer sits under the base's own capstone heading (`assistant (skill: build-prompt)`
  or `assistant (workflow: ready-to-build)`), so it is a capstone turn and stays out of evidence
  ([ADR-0030](./0030-gated-build-plan.md)).
- **Trust boundary.** The MCP `answer_plan` tool submits answers as Owner
  ([ADR-0033](./0033-mcp-idempotent-replay-and-plan-tools.md)): an agent's text is treated as the
  owner's, the same boundary `chat` already has. The tool description says to relay the owner's
  words, and the pointer turn notes `via MCP`.
- **A model re-plan can renumber ids.** Answers survive because they are carried by their `asked`
  text, not by id, and each dropped question is reported in the gate notes.
- **A `T#` answer does not carry across a model re-plan.** Only `Q#` answers are recorded as
  `answers` items that `answered_in_lineage` reads; a `T#` answer lives on its version (the task's
  `unblocked` field) and as an owner turn.
- **Invariants:** a base artifact's bytes never change; an identical resubmission returns the
  existing successor (`reused`) and writes nothing; re-gating is idempotent (`reset_derived` then
  `gates::run` renders the same bytes).

## Alternatives considered

- **Editing the plan in place.** Loses the history and breaks "a run is an artifact".
- **A separate answers file credited as Owner.** A second store the evidence gate would have to
  trust; an owner turn in the conversation is already the evidence source.
- **A model call per answer.** Non-deterministic, slow on a local model, and it would take a job
  slot for a write that needs none.
- **A `## user (plan: … answer Q6)` heading.** `parse_turn_heading` returns `User` only for the
  exact role `user`; the variant falls into `Other`, which `Evidence::new` credits as Foil, so the
  answer would not be Owner evidence and every exhaustive `TurnSource` match would change.
- **Redirecting R19 to a new page.** Breaks the existing route contract; the workbench lives on
  the artifact page instead.
- **A "fresh thread" option for a re-plan.** Recreates the original complaint (unrelated plans).

---

> ADRs are immutable once **Accepted**. To change a decision, write a new ADR that supersedes this
> one and update the Status line above.
