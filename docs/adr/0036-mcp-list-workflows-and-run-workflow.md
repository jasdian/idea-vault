# ADR-0036 — MCP `list_workflows` and `run_workflow`

- **Status:** Accepted
- **Date:** 2026-09-30
- **Deciders:** Owner
- **Amends:** [ADR-0024](./0024-mcp-server-inbound.md) — the tool catalog grows from 14 to 16 tools.
  [ADR-0029](./0029-mcp-moves-and-full-idea-read.md) — workflows are no longer "web-only".
- **Extends:** [ADR-0033](./0033-mcp-idempotent-replay-and-plan-tools.md),
  [ADR-0034](./0034-grounded-ranked-and-bounded-workflow-stages.md),
  [ADR-0035](./0035-workflows-as-markdown-and-the-workflow-book.md).

## Context

[ADR-0029](./0029-mcp-moves-and-full-idea-read.md) let an MCP client run a skill or a swarm and left
workflows web-only; [docs/13](../13-mcp-server-inbound.md) listed "a general `run_workflow` tool" under
Deferred, and only `build_plan` reached the ready-to-build workflow. With the workflow book of
[ADR-0035](./0035-workflows-as-markdown-and-the-workflow-book.md) and the stages of ADR-0034, the
book is the richest thing an MCP client could drive, and it is the one whose cost a client most needs
to know first: a run may take up to 32 model calls. The web route R22 already had the guards a client
needs (state check, name resolution), and ADR-0033 already had the replay rules for a long-running
result.

## Decision

We will expose the workflow book on the inbound MCP server as two tools.

1. **`list_workflows`** (sync, no arguments): the visible workflows of the current book in chip order,
   each with `name`, `description`, `use_when`, `avoid_when`, `stages` (the kinds, in order),
   `call_ceiling` (the ADR-0034 worst case), `needs_sources`, `capstone` and `source`. A capstone is
   listed, flagged, so a client learns it exists and where it runs.
2. **`run_workflow`** (`slug`, `name`, optional `idempotency_key`; long-running, task support
   `Optional`): claim, spawn and poll through the same Task-to-Job bridge as `run_skill`, using R22's
   guards (`guard_workflow`, `spawn_workflow_job`, ADR-0035). An unknown name, including one present
   only as an invalid owner file, is `invalid_params` before any job slot is claimed. A capstone
   (`ready-to-build`, or any workflow that chains a build-plan skill) is `invalid_params` with a
   pointer to `build_plan` with `audited:true`, which owns plan versioning and the owner's answers
   ([ADR-0032](./0032-plan-workbench-answers-and-versions.md), ADR-0033).
3. **The result** is the workflow's one appended turn, plus a second content item
   `{"artifacts": [slug…], "hint": "read each with get_artifact"}` listing the stage artifacts and the
   run record ADR-0034 wrote. The slugs are read back from the turn's trailing `Stage artifacts:` line;
   only the turn's last line is read, so a model-written look-alike earlier in the turn is never taken
   for it. A run that staged nothing lists none.
4. **Replay** follows ADR-0033 unchanged: the tool name `run_workflow` and the arguments hash key the
   cache, a served result is replayed to an identical call with no turn since (or the same
   `idempotency_key`), and the same key with different arguments is `invalid_params`.

## Consequences

- Easier: a client can discover what a workflow costs before calling it and read every stage artifact
  it produced without scanning `get_idea`.
- Harder: another long-running tool (six now) whose runtime is bounded only by the workflow's ceiling;
  a client should use task mode for it.
- Invariants: MCP adds no code path around R22's guards; a capstone is reached over MCP only through
  `build_plan`; the replay rules are not forked.
- Still web-only: extract, compact, tags, fork and source management.

## Alternatives considered

- **Let `run_workflow` run the capstone**: rejected. It would create a plan version without the
  lineage and answer handling `build_plan` guarantees.
- **Return only the turn**: rejected. The stage artifacts are the evidence of what a stage did; a client
  should not have to find them by scanning the whole idea.
- **A separate hash space for `run_workflow` replay**: rejected. ADR-0033's key already includes the
  tool name, so a `run_skill` and a `run_workflow` with equal arguments cannot collide.
- **Hide the ceiling until the run**: rejected. The point of `list_workflows` is to let the client
  decide.

---

> ADRs are immutable once **Accepted**. To change a decision, write a new ADR that supersedes this
> one and update the Status line above.
