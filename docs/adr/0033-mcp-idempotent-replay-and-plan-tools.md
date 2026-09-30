# ADR-0033 — MCP idempotent replay of served results, and the plan tools

- **Status:** Accepted
- **Date:** 2026-09-30
- **Deciders:** Owner
- **Amends:** [ADR-0028](./0028-optional-task-support-bounded-wait.md) — the "drop the reverse
  index after serving so the next call starts fresh" rule becomes "replay the served result under
  the rules below". [ADR-0024](./0024-mcp-server-inbound.md) — the tool catalog grows from 11 to 14
  tools (`build_plan`, `get_plan`, `answer_plan`), and `store_idea` gets an operation key.
- **Extends:** [ADR-0029](./0029-mcp-moves-and-full-idea-read.md); [ADR-0032](./0032-plan-workbench-answers-and-versions.md).

## Context

[ADR-0028](./0028-optional-task-support-bounded-wait.md) gives a plain call a bounded wait, then a
"still running" note; the client retries with the same arguments to collect. Once the outcome had
been served, the task registry forgot the slug, so the next identical call started a **second
model run**. That is wrong in the common case of a dropped response or a client that lost track:
for `run_skill build-prompt` the retry wrote a second, unrelated build plan
([ADR-0030](./0030-gated-build-plan.md)). Checked in code before the change: the forget ran right
after a terminal result was served; `op_key` was `None` for `store_idea`; and the result was
re-derived from the newest turn at read time, so a late `tasks/result` could return whatever turn
was newest by then. Separately, the plan workbench ([ADR-0032](./0032-plan-workbench-answers-and-versions.md))
needs an MCP surface.

## Decision

We will **render a result once, record it when served, and replay it** to an identical call.

1. **Render once.** A task's result is rendered when it first reaches a terminal state and stored
   on the entry (`TaskEntry::rendered`). `tasks/result` and the plain path both serve that value.
2. **Replay.** After a result was served, a plain or task call replays it when either (a) it
   carries the same explicit `idempotency_key`, or (b) its arguments hash matches and the idea's
   turn count has not changed since the run finished. The replayed text is prefixed
   `(replayed result of task N) `. In task mode a hit mints a task that is already terminal, so
   `tasks/get` then `tasks/result` work unchanged. An in-flight task keeps ADR-0028's reattach.
3. **Keys.** The same key with different arguments is `invalid_params`. A key never seen is a miss
   (a fresh key is how a client asks for a deliberate re-run). The args hash is over the arguments
   minus `idempotency_key`, with object keys sorted. The optional `idempotency_key` string is on
   `chat`, `store_idea`, `run_skill`, `run_swarm` and `build_plan`.
4. **What is cached.** Only Completed and Notice results, and only when the effect landed: a turn
   was appended past the count at claim (Chat, Skill, Swarm, Plan), or the idea is actually
   Stored (Store). Failed and Cancelled are never cached, so a retry after a failure runs again.
   A served entry is recorded under the args hash and, when a key was sent, also under the key.
5. **False-success guard.** A job that ends `Idle` but landed no turn (the web `/pending` poll
   consumed its Failed/Notice first) is rendered as an error, `finished but its result was
   consumed elsewhere — check get_idea`, and not cached.
6. **`op_key` for store** is `"store"`, so every kind reattaches on the same `Some` shape.
7. **Entries live 24 h, in memory only.** They are lost on restart; a lost entry degrades to a
   fresh run, never to a wrong result.

**Plan tools.**
- `build_plan {slug, audited?, idempotency_key?}` — `TaskSupport::Optional`, `TaskKind::Plan`,
  `op_key` `"quick"` or `"audited"`. It claims through the same seams as R48 (`guard_skill`
  + `spawn_skill_job` for `build-prompt`, or `guard_workflow` + `spawn_workflow_job` for
  `ready-to-build`); lineage is `finish`'s job. Its result is the head's `get_plan` JSON.
- `get_plan {slug, plan?}` — sync; the workbench view (defaults to the head).
- `answer_plan {slug, plan, answers: {"Q6": "…"}}` — sync, `spawn_blocking` into
  `workbench::answer` with `via: Mcp`; refused while a job runs; a `WorkbenchError` maps to
  `invalid_params` (a vault failure to an internal error). It is idempotent from the vault
  (`reused`), so it needs no cache.
- `run_skill` is unchanged; `get_artifact` now mentions build plans.

## Consequences

- A retry after a served result creates no job, turn or artifact.
- **A hash-key replay can swallow a deliberate identical re-run** made with no turn in between.
  The escape is a fresh `idempotency_key`, or re-planning with `build_plan`.
- **Restart caveat:** the cache is in memory, so a retry after a restart re-runs.
- **Trust boundary:** an MCP agent can submit answers as Owner through `answer_plan`; the server
  instructions say "relay the owner's own words to answer_plan; never compose them" and the
  pointer turn notes `via MCP`.
- Clients must treat the replay prefix as informational; the rest of the text is the original.
- The general `run_workflow` tool stays deferred; `build_plan` covers the one workflow that
  matters here.

## Alternatives considered

- **Keep forget-after-serve and tell clients to use task mode.** Leaves plain-call clients (the
  common case) with duplicate plans.
- **Cache by args hash only.** An identical `chat` message after an intervening turn is a new
  question; the turn-count check separates it, and a key covers the deliberate cases.
- **Re-derive the result at read time.** Returns whatever turn is newest then, not the task's own
  reply.
- **Persist replay entries.** Meaningless across a restart (the jobs behind them are gone) and
  a second store.

---

> ADRs are immutable once **Accepted**. To change a decision, write a new ADR that supersedes this
> one and update the Status line above.
