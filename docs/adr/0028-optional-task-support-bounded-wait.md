# ADR-0028 — Optional Task support for `chat`/`store_idea`: a bounded-wait plain call

- **Status:** Accepted
- **Date:** 2026-09-28
- **Deciders:** Owner
- **Amends:** [ADR-0024](./0024-mcp-server-inbound.md) — `chat`/`store_idea` move from
  `TaskSupport::Required` to `TaskSupport::Optional`. **Scoped exception to**
  [ADR-0010](./0010-ai-turns-as-background-jobs.md)'s "never block the request thread on a model
  call": the request thread may wait a short, fixed budget for a job it did not itself await.

## Context

ADR-0024 declared the two model-calling MCP tools `TaskSupport::Required`, so the `rmcp`
dispatch layer rejects a plain (non-task) `tools/call` for either with `-32601` before the
handler runs. That forced every client onto the poll-based Task lifecycle (SEP-1686), which is the
right shape for a long local-model turn — but it also made the two tools **uncallable** from any
MCP client that does not implement Tasks. Claude Code's own MCP client is one such client today:
it can list, read, search and create ideas over `/api/mcp`, but cannot continue a discussion or
store one, which is the whole point of the surface.

The Task path itself is not the problem and must not change: a Task-capable client gets an
immediate task id, polls at the server-suggested interval, and can cancel. What is missing is a
plain-call path that a Task-unaware client can use, without reintroducing the held-open-connection
shape ADR-0024 already rejected under "Blocking `call_tool` inline".

Two properties of the existing bridge (`web::mcp_server::tasks`) constrain any plain path:

1. `web::jobs::peek` is a **one-shot, consuming** read of a terminal slot. The bridge already
   caches the first terminal observation on the task entry (`TaskEntry::terminal`) precisely so
   `tasks/get` and `tasks/result` cannot consume each other's read. A plain path that polled
   `peek` on its own would be a third, uncached reader and would reopen that race.
2. `enqueue_task` owns the validate → claim → spawn business rules (idea exists, right state, slot
   not busy, reuse the HTTP route's own `spawn_chat_turn`/`run_store_work`). A plain path that
   copied them would drift.

## Decision

`chat` and `store_idea` are declared `TaskSupport::Optional`. A `tools/call` **with** `task:{}`
behaves exactly as under ADR-0024 (`enqueue_task` → `tasks/get` → `tasks/result`/`tasks/cancel`),
byte-for-byte. A **plain** `tools/call` is routed by `tools::call_sync` to
`TaskRegistry::call_sync_bounded`, which:

- runs the **same** `claim_and_spawn` the task path runs (extracted from `enqueue`, one copy of
  the business rules), mints a **real task id** into the same registry, and records a
  `slug → task_id` reverse index;
- polls that task through the **same** `observe()` terminal cache the Task RPCs use — never a
  second, independent `web::jobs::peek` — every `SYNC_POLL_INTERVAL` (250 ms) for at most
  `SYNC_WAIT_BUDGET` (3 s);
- on a terminal outcome inside the budget, returns the tool result built by the **same**
  `terminal_result` helper `tasks/result` uses (success, `Failed` → tool error, `Notice` folded
  into the message), then drops the slug's reverse-index entry so the next plain call for that
  idea starts a fresh job rather than replaying the served reply;
- on budget expiry, returns a **non-error** `CallToolResult` whose text says the job is still
  running (naming the task id) and to call the tool again with the same arguments. The job is
  **not** cancelled — it keeps running detached, like every other AI turn (ADR-0010).

A plain call for a slug whose newest registered task was claimed for the **same operation** —
same tool kind and, for `chat`, the same `message` (recorded on the task entry at claim time) —
**reattaches** to that task whether it is still `Working` (waits on it) or already terminal
(serves the cached result immediately); it never claims a second job or appends a second user
turn. This is what a naive client's retry looks like, and the terminal case is the common one: a
real local-model turn outlives the wait budget, so the retry usually arrives after the job has
finished. A different-kind or different-message entry is never reused; the call falls through to
a fresh claim, which fails fast with the usual "already busy" protocol error while the previous
job still holds the slot — the honest answer task mode gives too — or starts the new turn once it
does not. The reverse-index entry is dropped only when the plain path serves the outcome, so it
doubles as the "has this reply been collected yet" signal.

The constants are deliberately small. Three seconds is a "did it finish fast?" grace window that a
warm local model or a cached claude-code reply can meet, kept far below any HTTP client's request
timeout; it is not an attempt to cover a whole model turn synchronously, and it is unrelated to
`IDEA_VAULT_OLLAMA_TIMEOUT_SECS` (an inactivity bound inside the detached job).

The code keeps an explicit marker of the prior state: a comment on each `TaskSupport::Optional`
line in `tools::catalog()` and a paragraph in `tools.rs`'s module doc record that both tools were
`Required` until this ADR and why that was relaxed. Reverting is flipping the two enum values
back; the bounded-wait branch then becomes unreachable (rmcp rejects the plain call first) and can
be deleted at leisure.

## How this differs from the alternative ADR-0024 rejected

ADR-0024's "Blocking `call_tool` inline" alternative would have made the plain call the **only**
path, looping on `peek` for the full configured Ollama/claude-code timeout with no Task machinery.
That was rejected for three reasons: it capped every call at one hard-coded bound, it held the
HTTP request open for the whole model turn, and a slow model could not be polled from another
connection. This ADR keeps none of those properties: the Task path remains the primary, fully
featured one; the plain wait is a few seconds, not a model timeout; and the plain call still mints
a real task id, so a client that later learns Tasks — or the same client on its next plain retry —
picks the job up from any connection.

## Consequences

- Claude Code (and any Task-unaware MCP client) can now drive the full loop over `/api/mcp`. Its
  experience of a long turn is "call, get a still-running note, call again" — a polling loop
  driven by the model rather than by the client library. The tool descriptions and the server
  instructions string say so, so the model knows to retry rather than treat the note as a reply.
- A Task-capable client sees no change. The task-mode tests in `tests/mcp_server.rs` are the
  regression net for that claim.
- The request thread now waits up to `SYNC_WAIT_BUDGET` on a plain call. This is the one, scoped
  exception to ADR-0010's rule, and it is bounded by a constant, not by model behavior.
- The reverse index only tracks the **newest** task per slug. Older task ids stay reachable by
  id for a Task-capable client's polls; they are simply no longer what a plain retry reattaches
  to.
- A plain retry with the same arguments after the job finished receives the cached result
  (the terminal cache is shared, and the reverse-index entry survives until the plain path serves
  it — even if a Task-capable poll on the same task id already read it). A plain retry *after*
  the plain path served it starts a fresh job — so a client that repeats an identical message "to
  be sure" after receiving the reply will post a second turn. The tool description tells the
  model to retry only on the still-running note.
- A plain `chat` with a *different* message while the previous turn is still running is refused
  "already busy" as a protocol error, not silently swallowed; the new message is never persisted.

## Alternatives considered

- **Keep `Required`; teach clients Tasks.** Correct in principle, but out of idea-vault's hands:
  the owner cannot ship a Tasks implementation into Claude Code. Rejected as not actionable.
- **`Optional` with an *unbounded* inline wait on the plain path** (await the job to completion).
  Rejected: this is exactly the held-connection shape ADR-0024 rejected and ADR-0010 exists to
  prevent; a local model can run past any client's request timeout.
- **`Optional` with the plain path polling `web::jobs::peek` directly** (no task id minted).
  Rejected: it would be a third, uncached reader of a one-shot terminal slot and would reintroduce
  the false-`Idle` race the bridge's terminal cache was written to close; it would also duplicate
  `enqueue`'s validate/claim/spawn rules.
- **Return a JSON-RPC error on budget expiry** instead of a success-with-note. Rejected: a
  Task-unaware client's model would read the error as a failed turn and might resend the message,
  spawning a duplicate; the non-error note plus reattach-on-retry is the shape that makes a naive
  retry safe.

---

> ADRs are immutable once **Accepted**. To change a decision, write a new ADR that supersedes this
> one and update the Status line above.
