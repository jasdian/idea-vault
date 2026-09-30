# ADR-0035 — Workflows as markdown files and the workflow book

- **Status:** Accepted
- **Date:** 2026-09-30
- **Deciders:** Owner
- **Amends:** [ADR-0022](./0022-skills-as-markdown-and-the-skill-book.md) — the same file-and-book
  model now covers workflows, and a reload re-reads both.
- **Extends:** [ADR-0034](./0034-grounded-ranked-and-bounded-workflow-stages.md),
  [ADR-0030](./0030-gated-build-plan.md) (the capstone).

## Context

Workflows were a closed Rust list: `Stage` was a `Copy` enum over `&'static` data, the three
built-ins were statics, and `get_workflow` looked a name up in three places. An owner could add a
skill (ADR-0022) but not a workflow, and adding the four stages of
[ADR-0034](./0034-grounded-ranked-and-bounded-workflow-stages.md) as more statics would have made the
call ceiling, the caps and the cross-checks with skills impossible to validate for anyone but the
maintainer. The workflow list also fed three consumers that each assumed a static: the idea page's
chips (filtered on the name `ready-to-build`), spine coverage, and a test that iterated the
built-ins. The ready-to-build capstone is special: its transcript turn is a pointer, and
`CAPSTONE_TURNS` in `domain::evidence` is a pure list of the two names whose turns are never
evidence, so a capstone under an arbitrary owner name would leak model-authored plan text into
memory evidence.

## Decision

We will define every workflow as a markdown file and treat the pair (skills, workflows) as one book.

1. **Files.** Frontmatter `name`, `description`, `use_when`, `avoid_when`, `hidden`, `stages` (a list
   of mappings each tagged `kind:`), then a markdown body that explains the workflow to the owner and
   is never sent to a model. Built-ins live in `src/concepts/workflows/*.md`, compiled in with
   `include_str!`; their file order is the chip order. Owner files live in
   `IDEA_VAULT_WORKFLOWS_DIR`, default `<vault>/.workflows/`, a dot-dir reindex never enters: app
   configuration, not vault truth.
2. **Loading follows ADR-0022 exactly.** An owner file whose name matches a built-in replaces it in
   place; a new name is appended in file-name order; the name must be a slug equal to the file stem;
   a file over 32 KiB is refused.
3. **Parsing by hand-dispatched kind.** The frontmatter is parsed with `deny_unknown_fields`; each
   stage value must be a mapping; `kind` is taken off and the rest is deserialised into that kind's
   own struct (also `deny_unknown_fields`). This is deliberately not a serde internally-tagged enum,
   whose interaction with `deny_unknown_fields` is unreliable: a typo must surface, not default.
4. **Cross-registry validation** is a pure function of (frontmatter, skill registry) and reports one
   `WorkflowIssue` per broken rule, each naming `stages[i] (<kind>)`: the kind is known; every skill
   resolves; extract-stage skills appear only in harvester `fan_out` or `loop` steps; the internal
   skills `ground-read` and `panel-score` are not owner steps; an angle is read only by panel
   proposers; at most eight stages; every cap of ADR-0034; at most one Ground and it first; a Panel is
   followed by Synthesize or Audit then Synthesize; an Audit follows a fan-out, loop or panel; a Refine
   follows an Audit; a build-plan chain is last; the ceiling is at most 32.
5. **Capstone is derived and named.** A workflow is a capstone when it chains a build-plan skill; that
   is allowed only under the name `ready-to-build`. An owner forks the capstone by overriding that
   name, which keeps `CAPSTONE_TURNS` a correct pure list.
6. **Invalid files are book issues.** An invalid owner file is listed on the book and the built-in of
   the same name stays active; a name present only as an invalid file is a 404 for R22. The run-time
   `UnknownSkill` check stays in `run_workflow` as a backstop.
7. **One pair, reloaded together.** `LiveWorkflows` holds `(skills, workflows)` as one `Book`.
   `POST /skills/reload` (R34) reloads the skills, then revalidates the workflows against that fresh
   skill snapshot, and swaps the pair in whole. A job takes the pair once (`snapshot()`) and runs
   entirely against it, so a reload mid-run, or a workflow validated against different skills than the
   ones it runs, is impossible.
8. **The workflow book and R49.** The skill book page also lists every workflow (stages, per-stage and
   total call ceiling, waves at K, source, an issues banner). `GET /skills/workflow/{name}` (R49)
   renders one in full: its stages, a Panel's rubric, the body, and the definition file as loaded, to
   copy into `IDEA_VAULT_WORKFLOWS_DIR` to fork it. The idea page's chips come from the same book:
   every visible non-capstone workflow, each with the ceiling and waves in its tooltip and, when the
   idea has no sources, a hint on a workflow that opens with Ground.
9. **Migration in one change.** `interrogate` and `steelman-then-attack` are converted to markdown with
   behaviour pinned by a migration test; `ready-to-build` becomes a file (gaining Ground, ADR-0034);
   there is no parallel static path.

Reader-facing changes to shared seams: `MAX_ANGLES` moves from `web::routes::memory` to
`concepts::swarm`, so a workflow's `fan_out` is checked against it at load; `run_workflow` takes a
`RunCtx` and carries no clippy allowance (the floor drops from 6 to 5); R22's guard and spawn are split
into `guard_workflow` and `spawn_workflow_job`, reused by MCP ([ADR-0036](./0036-mcp-list-workflows-and-run-workflow.md)).

## Consequences

- Easier: an owner can add, tweak or fork a workflow with a text editor and see, before running it,
  what it costs and what is wrong with it; the maintainer adds a built-in by adding a file.
- Harder: every consumer of the workflow list reads the live book, not a static; a skill edit can
  invalidate a workflow, by design, and shows on the book at the next reload.
- Invariants: an invalid file never replaces a working built-in; a job sees one consistent pair; the
  only capstone is `ready-to-build`; an owner workflow is app config and is never indexed.
- Not done: no file watcher (reload is a button); no per-workflow enable flag beyond `hidden`.

## Alternatives considered

- **Keep workflows as Rust statics and only add stages**: rejected. Owners could not add or tune one,
  and the validation of ADR-0034's caps would have lived only in review.
- **A serde internally-tagged `Stage` enum**: rejected, for the `deny_unknown_fields` reason above.
- **Let any owner workflow be a capstone**: rejected. `CAPSTONE_TURNS` would have to become dynamic
  state, and a mis-named capstone would make plan text count as memory evidence.
- **Reload skills and workflows independently**: rejected. A workflow could then be validated against
  a skill snapshot different from the one a job runs with.
- **Store owner workflows in the index**: rejected, by the same rule as owner skills: app config the
  reindex must never depend on.

---

> ADRs are immutable once **Accepted**. To change a decision, write a new ADR that supersedes this
> one and update the Status line above.
