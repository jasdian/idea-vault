# 13 — Inbound MCP server

> How idea-vault exposes **itself** as a Model Context Protocol server at `POST /api/mcp`, so an
> external LLM client (Claude Desktop/Code, or any Streamable-HTTP MCP client) can list ideas,
> read one, continue a discussion, and store it — the mirror image of
> [ADR-0018](./adr/0018-mcp-servers.md)'s **outbound** registry (idea-vault calling *other* MCP
> servers). Decision record: [ADR-0024](./adr/0024-mcp-server-inbound.md). This doc is both the
> feature reference and a general-purpose **cookbook** for wiring an MCP server onto an axum app
> that already runs long AI calls as background jobs — the pattern generalizes past idea-vault.

## Why this exists

idea-vault's core AI turns (`chat`, `store`, `skill`, `swarm`, `workflow`, `extract`, `compact`)
run as **detached background jobs** (ADR-0010, `web::jobs`): a route claims a per-idea job slot,
spawns a `tokio::spawn` task, and returns immediately; the web UI polls `GET /idea/{slug}/pending`
until the job resolves. That shape was chosen so a model call — which can run for tens of seconds
on a local model — never depends on one HTTP request/response staying open. Any inbound MCP
surface has to respect that same constraint, and the [Model Context Protocol's **Tasks**
primitive](https://modelcontextprotocol.io/specification/2025-11-25/basic/utilities/tasks)
(SEP-1686) turns out to match it almost exactly: a `tools/call` made with a `task: {}` parameter
returns immediately with a task id; the client polls `tasks/get`/`tasks/result` afterward. That's
the load-bearing idea behind everything below.

## Module layout

```
web::mcp_server
├── mod.rs       — router() : assembles the rmcp StreamableHttpService + AuthLayer, mounts /api/mcp
├── auth.rs      — single-token Bearer AuthLayer/AuthMiddleware (Tower Layer/Service pair)
├── handler.rs   — IdeaVaultMcpServer : the rmcp::ServerHandler impl
├── tools.rs     — the MVP tool catalog + synchronous tool dispatch (list_ideas, get_idea, search,
│                   create_idea, reopen_idea)
├── tasks.rs     — TaskRegistry : the Task↔Job bridge for chat/store_idea
└── prompts.rs   — a small canned prompt catalog
```

Lives under `web` (not a new top-level module) because it depends on `AppState` and everything
`web` already depends on — no new module-dependency edge. It is a **third** same-topic module
alongside `crate::mcp` (the outbound server registry) and `crate::ai::mcp` (the outbound client):
three modules, one topic, opposite directions, each staying in its own lane.

## Router assembly (`mod.rs`)

```rust
pub fn router(state: AppState, token: String) -> Router {
    let handler = IdeaVaultMcpServer::new(state);
    let service = StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    Router::new()
        .route_service("/api/mcp", service)
        .layer(AuthLayer::new(token))
}
```

`app::build_router` merges this in only when `Config::mcp_server_token` is `Some` — unset means
the route is never mounted, not mounted-but-open (see ADR-0024's "detect absence, disable the
feature" reasoning). Path choice matters: idea-vault's *outbound* registry UI already owns `/mcp`
and `/mcp/{name}/*`, so the inbound protocol endpoint needed its own namespace — `/api/mcp` was
free. Pick whatever prefix isn't already claimed on your own router before copying this pattern.

`IdeaVaultMcpServer` is constructed **once** here, not per-request or per-session — its `tasks:
Arc<TaskRegistry>` field must be shared across every session, since a client may reconnect between
`enqueue_task` and a later `tasks/get`. The `move || Ok(handler.clone())` factory `rmcp` calls per
session clones the outer struct cheaply (it's just an `AppState` clone plus an `Arc` clone).

## Auth (`auth.rs`)

A Tower `Layer`/`Service` pair, adapted from the sibling `mcp-server` repo's own `AuthLayer` but
simplified: idea-vault is solo, so there's no multi-user `CredentialsStore` — just an `Arc<str>`
token and a `Bearer <token>` equality check. On success the request passes through unchanged (no
identity is injected into extensions, because nothing downstream needs one). On failure, a
JSON-RPC-shaped 401 body is returned directly from the middleware, before the request ever reaches
`rmcp`.

**Cookbook note:** if your server *does* have multiple callers/identities, inject the resolved
identity into the request's extensions in the middleware (`req.extensions_mut().insert(...)`), and
pull it back out inside `call_tool`/`enqueue_task` via
`context.extensions.get::<http::request::Parts>().and_then(|p| p.extensions.get::<YourIdentity>())`
— `rmcp` copies the inbound `http::request::Parts` into every `RequestContext`, which is how a
Tower-layer-populated identity survives into the MCP handler. See `mcp-server`'s
`CosmicMcpServer::call_tool` for a worked example; idea-vault doesn't need this because there's
only ever one caller.

## The `ServerHandler` (`handler.rs`)

`IdeaVaultMcpServer` implements `rmcp::ServerHandler`. The methods that matter:

- `get_info` — capabilities (`enable_tools().enable_prompts()`, **not** `enable_resources()` —
  the MVP doesn't expose vault files as MCP resources yet) plus a short instructions string.
- `get_tool` — returns one tool's full definition by name; `rmcp`'s own dispatch layer calls this
  **before** `call_tool`/`enqueue_task` to enforce each tool's declared `TaskSupport` (see below).
  Returning `None` here (the trait default) would silently skip that enforcement.
- `list_tools` / `call_tool` — the synchronous surface, delegated to `tools.rs`.
- `enqueue_task` / `get_task_info` / `get_task_result` / `cancel_task` — the Task lifecycle,
  delegated to `tasks::TaskRegistry`.
- `list_prompts` / `get_prompt` — delegated to `prompts.rs`.

## Marking a tool long-running (`tools.rs`)

```rust
Tool::new("chat", "...", schema)
    .with_execution(ToolExecution::new().with_task_support(TaskSupport::Required))
```

`TaskSupport::Required` means `rmcp`'s dispatch layer itself rejects a plain (non-task)
`tools/call` for that name with `-32601 Method not found`, **before `call_tool` ever runs** — so
`chat`/`store_idea` never actually reach `tools::call_sync` in practice; that function only
handles them defensively. This is the cheapest possible way to force a client onto the polling
lifecycle for exactly the tools that need it, with zero manual branching in your own handler code.
The other three tool-support values are `Forbidden` (the default — synchronous only) and
`Optional` (client's choice) if you want a tool callable either way.

## The Task↔Job bridge (`tasks.rs`)

The one genuinely new piece of machinery, and the reusable idea if you're copying this pattern
onto a different app with its own "background job, polled by the client" system:

1. **`enqueue_task`** validates the call synchronously (idea exists, right state, not already
   busy) so a doomed call fails fast as a protocol error rather than minting a task the client has
   to poll just to learn it was doomed. On success it claims the job slot, spawns the work exactly
   like the HTTP route does (reusing the *same* `pub(crate)` functions the route calls —
   `chat::spawn_chat_turn`, `memory::run_store_work` — never a second copy of the business logic),
   mints a task id, and records `task_id → (idea slug, tool kind)` in an in-memory map.
2. **`get_task_info`** (`tasks/get`) looks up the slug, calls the job system's own status peek
   (`web::jobs::peek`), and translates its states into MCP `TaskStatus`: `Running → Working`,
   `Idle → Completed`, `Failed → Failed`. No changes to the job module itself — this bridge
   consumes it purely through existing public functions.
3. **`get_task_result`** (`tasks/result`) is the interesting part: idea-vault's job system tracks
   *status* but never stores a *return value* (the web UI doesn't need one — it just re-renders
   the transcript from disk once a job finishes). So once the job is idle, this function
   **re-derives the tool's result by re-reading the vault** — the newest conversation turn for
   `chat`, the fresh frontmatter for `store_idea`. This isn't a workaround; the vault is the
   source of truth (ADR-0002) regardless of which surface asks, so re-reading it after completion
   is the *correct* way to answer "what happened," not a shortcut around a missing feature.
4. **`cancel_task`** (`tasks/cancel`) forwards straight to the job system's own `cancel`.

If you adapt this pattern for a job system that already returns a value from its completion
callback, `get_task_result` gets simpler — you'd store that value in the task-id map instead of
re-deriving it. The re-derive step here is specific to idea-vault's markdown-is-truth design, not
an inherent part of the Task↔Job bridge idea.

## Prompts (`prompts.rs`)

A tiny static catalog (`continue-discussion`, `new-idea`) in the same declarative shape as tools:
name, description, typed arguments, a text template with `{arg}` placeholders filled at `get`
time. Prompts are pure text — no vault I/O — they just name which tools an MCP client's "/" picker
should drive next.

## Testing pattern (`tests/mcp_server.rs`)

Build the real `axum::Router` via `app::build_router(state)` **once per test**, then drive it with
`tower::ServiceExt::oneshot`, reusing the same `Router` (via `.clone()`) for every request in that
test — building a fresh router per request would mint a fresh, empty `LocalSessionManager` and
`TaskRegistry` and break the session/task lifecycle. The flow every test follows:

1. **Handshake**: POST `initialize` → capture the `mcp-session-id` response header → POST
   `notifications/initialized` carrying that session id. Every later request in the test carries
   the same session id header.
2. Response bodies are SSE (`text/event-stream`) by default — pull the JSON-RPC payload out of the
   `data: ` line (`extract_json` helper), falling back to parsing the raw body as JSON for the
   rare non-SSE response.
3. For a task-mode call: POST `tools/call` with `"task": {}` → read `result.task.taskId` → poll
   `tasks/get` until `result.status != "working"` → POST `tasks/result` → assert on the payload
   **and** on the on-disk vault state (markdown-is-truth: a test that only checks the MCP response
   without checking `conversation.md`/`idea.md` on disk hasn't actually verified anything durable
   happened).
4. AI paths reuse the existing `support::spawn`/`ChatScript` mock Ollama server — never a live
   model, exactly like every other AI-path test in this suite (docs/10-testing-strategy.md).

## Config

| Var | Default | Purpose |
|---|---|---|
| `IDEA_VAULT_MCP_TOKEN` | unset (feature off) | Bearer token gating `/api/mcp`. Unset/blank: not mounted. No live retuning — a restart is needed to change it, since the route is mounted once at boot. |

See [docs/12-deployment.md](./12-deployment.md) for the full env var contract table, and
[ADR-0024](./adr/0024-mcp-server-inbound.md) for why absence disables the feature rather than
defaulting to open.

## Scope: what's in, what's deferred

**In (this pass):** `list_ideas`, `get_idea`, `search`, `create_idea`, `chat`, `store_idea`,
`reopen_idea`, plus the two-prompt catalog.

**Deferred:** skills/swarm/workflow/extract/compact tools, fork/tags/sources-management/delete-*
tools, MCP `resources` (idea.md/conversation.md as `resources/read` + `resources/subscribe`
push-on-update — `rmcp` supports this; it's additive and independent of the current tool set), a
stdio transport variant, and multi-user/per-caller credential scoping (not needed while idea-vault
is solo). Extending the tool catalog is mechanical — add an entry to `tools::catalog()` and a
matching arm in `tools::call_sync` (or, for another long-running move, a case in
`tasks::TaskRegistry::enqueue` alongside `chat`/`store_idea`) — but do it alongside an ADR-0024
amendment noting the expanded scope, per that ADR's own consequence note.
