# ADR-0024 — Inbound MCP server: exposing idea-vault to LLM clients

- **Status:** Accepted — scope amended by [ADR-0028](./0028-optional-task-support-bounded-wait.md),
  [ADR-0029](./0029-mcp-moves-and-full-idea-read.md) and
  [ADR-0033](./0033-mcp-idempotent-replay-and-plan-tools.md)
- **Date:** 2026-09-28
- **Deciders:** Owner

## Context

idea-vault already **consumes** external MCP servers (ADR-0018: `crate::mcp` is a persistent
registry of owner-added tool servers, `crate::ai::mcp` is the outbound Streamable-HTTP client,
surfaced at the `/mcp*` management UI). The owner also wants the reverse: driving idea-vault
itself — listing ideas, reading one, continuing a discussion, storing it — from an external LLM
client (Claude Desktop/Code) the same way any other MCP-backed tool works, not only through the
HTMX web UI. Nothing in the repo addressed this before this ADR; it is greenfield.

Two constraints from the existing architecture bear directly on the design:

1. **AI turns run as detached background jobs, not synchronous calls** (ADR-0010): `chat`,
   `store`, `skill`, `swarm`, `workflow`, `extract`, and `compact` all claim a per-idea job slot
   (`web::jobs`), spawn a `tokio::spawn` task, and return immediately; the web UI polls
   `GET /idea/{slug}/pending` until the job resolves. A local-model turn can run well past a
   typical HTTP client's request timeout, and ADR-0010 explicitly chose this shape over holding a
   connection open (superseding ADR-0004's SSE-streaming decision) so navigating away — or a
   dropped connection — can never cancel generation. Any MCP tool wrapping `chat`/`store_idea`
   must respect this: it cannot simply `.await` a model call inline inside `tools/call`.
2. **Markdown is truth; SQLite is a rebuildable index** (ADR-0002). Any new write path (an MCP
   `create_idea`/`chat`/`store_idea` tool) must go through the same vault-write-then-reindex
   sequence the HTTP routes use, not a shortcut that leaves the index stale.

Two owner repos outside idea-vault (`mcp-server`, `mcp-server-comtegra`) already implement an MCP
server on axum with the official `rmcp` SDK and a Tower `AuthLayer` Bearer gate — a proven pattern
to build from rather than invent from scratch. Reading `rmcp` 1.8.0's own source turned up a
directly relevant primitive: **MCP Tasks** (SEP-1686) — a `tools/call` made with a `task: {}`
parameter returns an immediate `CreateTaskResult{taskId, status: working}`; the client then polls
`tasks/get`/`tasks/result` (and can `tasks/cancel`) over separate requests until the task
completes. This is a poll-based lifecycle, not a held-open connection — the same shape ADR-0010
already committed to for the web UI.

## Decision

We will mount an inbound MCP server at **`POST /api/mcp`** on idea-vault's existing `axum::Router`
(`app::build_router`), using the official `rmcp` SDK's `ServerHandler` trait over its
`StreamableHttpService` transport — reusing the live `AppState` directly rather than a separate
process or a stdio binary. `/api/mcp` is a distinct path from the existing `/mcp*` **outbound**
registry-management UI (`web::routes::mcp`); the new code lives in `web::mcp_server`, a third
same-topic module alongside `crate::mcp` (outbound registry) and `crate::ai::mcp` (outbound
client), continuing that split rather than overloading either.

The mount is gated by a **single static Bearer token** (`IDEA_VAULT_MCP_TOKEN`), via a Tower
`AuthLayer` adapted from the `mcp-server` repo's own (simplified: idea-vault is solo, so there is
no per-user `CredentialsStore`, just a boolean token match). If the token is unset, `/api/mcp` is
**not mounted at all** — an unauthenticated tool surface is not a safe default, so absence
disables the feature rather than falling back to open, mirroring the "detect absence, surface a
clear state" discipline already applied to Ollama's own absence (D20).

The MVP tool set is deliberately lean, matching what the owner asked for first: `list_ideas`,
`get_idea`, `search`, `create_idea`, `chat`, `store_idea`, `reopen_idea` — plus a small prompts
catalog (`continue-discussion`, `new-idea`). Skills, swarm, workflows, extract, compact, fork,
tags/sources management, and delete-* tools, and MCP `resources` (idea.md/conversation.md as
`resources/read` + `resources/subscribe`) are explicitly deferred to a follow-up pass.

`chat` and `store_idea` are declared `TaskSupport::Required` in the tool catalog: the `rmcp`
dispatch layer itself rejects a plain (non-task) `tools/call` for either with `-32601` before our
handler ever runs, so a client is forced onto the Task lifecycle for exactly the two tools that
run a model call. *(Amended by [ADR-0028](./0028-optional-task-support-bounded-wait.md): both
tools are now `TaskSupport::Optional` — the task path below is unchanged, and a plain call takes a
short bounded wait on the same registry instead of being rejected.)* A new
`web::mcp_server::tasks::TaskRegistry` bridges an MCP task id to an idea
slug and a tool kind, and translates `web::jobs::peek`'s `Pending` states into MCP `TaskStatus`
(`Running → Working`, `Idle → Completed`, `Failed → Failed`). `web::jobs::peek` is a **one-shot,
consuming** read of a terminal slot (correct for its one HTTP poll endpoint) — but the Task
lifecycle asks about the same task through two separate RPC methods (`tasks/get` then
`tasks/result`), and a client may poll `tasks/get` more than once, so `TaskRegistry` caches the
terminal outcome the first time either method observes it (`TaskEntry::terminal`) rather than
reading `web::jobs::peek` more than once per task. `web::jobs::Job` carries no return payload, so
`tasks/result` re-derives the tool's result by re-reading the vault once the cached outcome is
`Completed`/`Notice` (the newest conversation turn for `chat`, the fresh frontmatter for
`store_idea`) — the vault is truth anyway, so this is the correct source, not a workaround. No
changes were made to `web::jobs.rs` itself; the bridge consumes it purely through its existing
public functions.

The five remaining tools (`list_ideas`, `get_idea`, `search`, `create_idea`, `reopen_idea`) are
plain synchronous `call_tool` handlers. `create_idea` and `reopen_idea` call small `pub(crate)`
core functions (`ideas::create_idea_core`, `memory::reopen_idea_core`, `memory::guard_can_store`)
extracted from the existing HTTP handlers, so the MCP tool and the HTTP route run identical
business logic — including the vault-write-then-reindex sequence — rather than a second,
divergent copy of it.

## Consequences

- A client can drive idea-vault's core loop (list, read, create, chat, store) over MCP, from the
  same running server the owner's browser talks to — no separate process, no duplicated
  vault/index/job state.
- The `chat`/`store_idea` Task lifecycle means a well-behaved MCP client polls `tasks/get` at the
  server-suggested interval (1.5s) rather than holding a connection open; this keeps `/api/mcp`
  consistent with ADR-0010's reasoning instead of reintroducing the SSE-streaming shape ADR-0004
  was superseded to avoid.
- `IDEA_VAULT_MCP_TOKEN` is a new required piece of configuration for anyone who wants this
  feature; its absence is silent-by-design (no route mounted, no error), which downstream tooling
  (health checks, `doc-sync`'s env-var table check) must account for as "off," not "misconfigured."
- A business-error asymmetry a client author must know about: the five synchronous tools
  (`get_idea`, `search`, `create_idea`, `reopen_idea`, `list_ideas`) surface a bad request (unknown
  slug, wrong state, …) as `CallToolResult::error` — a tool-result error, visible to the model
  in-band. `chat`/`store_idea`'s `enqueue_task`, by contrast, surfaces the *same class* of error
  (unknown slug, wrong state, idea already busy) as a JSON-RPC protocol error
  (`McpError::invalid_params`), by design (fail fast — a doomed call should never mint a task the
  client has to poll just to learn it was doomed). This is deliberate, not an oversight; a future
  pass could unify the two shapes, but should not silently drift them further apart.
- The MVP tool set is intentionally incomplete. A follow-up ADR (or an amendment here, since this
  is Accepted) is expected once skills/swarm/workflow/extract/compact tools and MCP resources are
  added — do not silently expand `web::mcp_server::tools::catalog()` without updating this ADR's
  "Consequences"/scope note.
- `web::routes::ideas`, `web::routes::memory`, and `web::routes::chat` each gained one `pub(crate)`
  seam (`create_idea_core`, `reopen_idea_core`/`guard_can_store`/`run_store_work`,
  `spawn_chat_turn`) so `web::mcp_server` could reuse them — a small, deliberate widening of their
  visibility, not a design change to those modules.

## Alternatives considered

- **A separate stdio-launched binary** (the common Claude Desktop local-tool pattern: the client
  spawns `idea-vault mcp` itself, no network port). Rejected for this pass: it would need its own
  process wiring to the same vault dir and SQLite index the web server already has open, risking
  two processes writing `index.db` concurrently, and the owner already runs idea-vault as a
  long-lived localhost service (bare `cargo run` or the Compose stack) — mounting onto the
  existing server has no such risk and no duplicated boot path. Not ruled out permanently; revisit
  if a client that cannot speak Streamable HTTP is needed.
- **Progress notifications (`notifications/progress`) instead of the Tasks primitive**, holding
  the SSE connection open for the whole `chat`/`store_idea` call and streaming progress down it.
  Rejected: this is exactly the held-open-connection shape ADR-0010 moved away from for the web
  UI (superseding ADR-0004) — a dropped or navigated-away connection would still be fine
  server-side (the job keeps running detached either way), but the *client's* MCP call would
  error out mid-flight with nothing to poll afterward, unlike the Task lifecycle.
- **Blocking `call_tool` inline** (claim the job, then loop `web::jobs::peek` inside the async
  handler until done, bounded by the configured Ollama/claude-code timeout, no Task machinery at
  all). Simpler to implement, but caps every `chat`/`store_idea` call at whatever the single
  hard-coded wait bound is, forces the HTTP request/response to stay open for the whole model
  call (reintroducing the held-connection risk above), and gives a slow local model no way to
  report progress or be polled from a different connection. Rejected in favor of Tasks, which
  solves all three.
- **Per-user credentials (`CredentialsStore`, mirroring `mcp-server`'s multi-tenant auth)**.
  Rejected: idea-vault is a solo tool (CLAUDE.md's "solo ideation"); there is exactly one caller
  identity to gate, so a single static token is sufficient and a multi-user credential file would
  be unused complexity.
- **Reusing the `/mcp` path** for the inbound protocol endpoint, since that is the conventional
  MCP mount point in the sibling repos. Rejected: `/mcp` and `/mcp/{name}/*` are already the
  outbound registry-management UI's routes (`web::routes::mcp`); reusing the path would either
  collide or force renaming a stable, already-shipped route. `/api/mcp` was free and unambiguous.

---

> ADRs are immutable once **Accepted**. To change a decision, write a new ADR that supersedes this
> one and update the Status line above.
