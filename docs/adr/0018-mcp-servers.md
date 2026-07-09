# ADR-0018 — MCP server management

- **Status:** Accepted
- **Date:** 2026-07-08
- **Deciders:** owner

## Context

[ADR-0017](./0017-web-access-tools.md) gave the foil two hardcoded tool leaves — `web_search` and
`fetch_url` — on the Ollama path, and allow/deny of the CLI's own `WebSearch`/`WebFetch` on the
claude-code path. That covers the general web, but not an arbitrary external system the owner
already runs an [MCP](https://modelcontextprotocol.io) (Model Context Protocol) server for — a
project tracker, a private filesystem index, whatever. The owner wants to point the foil at zero or
more such servers, on either backend, without hardcoding any one server into the crate.

MCP servers are owner infrastructure, not idea-vault infrastructure: they need a name, a
Streamable-HTTP URL, an optional bearer token, and an enabled/disabled flag, and that list has to
survive a restart, be editable from the web UI, and be visible to both `LlmBackend` implementations
identically in spirit — the same shape ADR-0017 established for `web_access`.

## Decision

We will add an owner-managed **MCP server registry** plus a protocol client, split across two new
modules to keep the dependency graph acyclic, a live management page, and a bridge in `ai::backend`
that offers each enabled server's tools to whichever backend is active.

- **Persistence — `crate::mcp` (pure config, `std` + `serde` only).** `McpRegistry` holds
  `Vec<McpServerConfig>` (`name`, `url`, `bearer_token: Option<String>`, `enabled: bool`) mirrored to
  one JSON file, `<vault>/.mcp-servers.json` by default, overridable via `IDEA_VAULT_MCP_CONFIG`
  ([config.rs](../../src/config.rs)). This file is **app config, not vault truth**: it lives inside
  the vault directory only because that's the one host-persistent bind mount in a containerized run
  (see [12-deployment](../12-deployment.md)), not because it's part of the idea corpus. It is
  **structurally invisible to reindex** — `vault::walk` only admits directories containing an
  `idea.md`, so a top-level dotfile is never enumerated, walked, or indexed; losing it costs the
  owner a re-add of server URLs, never any idea content. Every mutation (`add`/`update`/`set_enabled`
  /`remove`) persists via a same-directory tmp+rename, matching `vault::store::write_atomic`'s
  crash-safety without a `vault` dependency (implemented locally, see the module doc for why it
  isn't shared). Missing/unparsable file degrades to an empty registry with a `tracing::warn`, never
  a boot crash. Server names are restricted to `[a-z0-9-]` because the name is spliced into
  model-facing tool names (`mcp__<name>__<tool>` / `mcp__<name>`) — see the wiring below. The token
  is write-only from the browser: `GET /mcp/{name}/edit` never echoes a stored token back, only
  `has_token`; a "clear token" checkbox on the edit form disambiguates a blank field's three-way
  intent (keep / clear / replace) via `TokenChange`, since `Option<String>` alone can't tell "leave
  it alone" from "I have nothing to type."
- **Wire client — `ai::mcp` (pure protocol, no config knowledge).** `McpClient`/`McpSession`
  implement the Streamable-HTTP MCP transport: one POST per JSON-RPC call
  (`initialize` → optional `Mcp-Session-Id` on every later call → `tools/list` / `tools/call`), every
  POST carrying `Accept: application/json, text/event-stream` and an `Authorization: Bearer <token>`
  header only when configured. A server may answer with plain `application/json` or (the common
  default) `text/event-stream` carrying one JSON-RPC message; both are parsed. **Errors are content,
  not failure**, mirroring ADR-0017's discipline: transport errors, timeouts, and JSON-RPC `error`
  objects come back as a readable `Err(String)`, and a tool call that completed with `isError: true`
  comes back `Ok`, prefixed `"tool error: "`, so the model can read and route around it instead of
  the turn failing outright. The one protocol-aware retry: a `404` on any non-`initialize` call means
  the session expired (or the server never issued one), so the client re-initializes once and retries
  the call once before giving up.
- **The bridge — `ai::backend`, and only `ai::backend`.** This is the deliberate module split: `mcp`
  knows *which servers exist and are enabled* but nothing about the wire protocol; `ai::mcp` knows
  *how to talk to one server* but nothing about which servers exist; `ai::backend`'s tool loop is the
  one place that combines the two — connecting to every enabled server, listing its tools, and
  routing a model's tool call to the right session. `mcp` must never import `ai`, and `ai::mcp` must
  never import `crate::mcp`; the dependency is one-way, `ai::backend → {ai::mcp, crate::mcp}`, no
  cycle. On the **Ollama path**, enabled servers' tools are mangled `mcp__<server>__<tool>` and
  merged into the same bounded tool-calling loop `web_search`/`fetch_url` already run in
  ([ADR-0017](./0017-web-access-tools.md)) — an MCP server being enabled is now, alongside
  `web_access`, a second reason that loop runs instead of a plain single-shot call. On the
  **claude-code path**, enabled servers become a generated `--mcp-config` temp-file JSON blob
  (`{"mcpServers": {...}}`) passed to the CLI, plus one `mcp__<name>` allow-list entry per server
  (the CLI expands a bare prefix to every tool the server advertises) combined with
  `--strict-mcp-config` so the CLI never silently picks up an MCP config from elsewhere on the host.
- **The `/mcp` management page** (`GET`/`POST /mcp`, [09-web-ui](../09-web-ui.md) route map):
  add/edit/update/toggle/delete a server, mirroring the Settings page's HTMX-partial shape but backed
  by `McpRegistry` instead of `LlmSettings`. **Probe is deliberately inline, not a job.** Every other
  network-touching route in the app that could be slow goes through `web::jobs`
  ([ADR-0010](./0010-ai-turns-as-background-jobs.md)) because a *model* call can run for minutes and
  must survive the browser navigating away; an MCP probe is one bounded HTTP round trip already capped
  by `ai::mcp`'s own timeouts (3s connect, 15s request), so awaiting it directly in the handler is
  both simpler and fast enough to read as synchronous — it hx-targets only the probed row's status
  slot, not the whole panel.
- **The usage meter's "(+N KB tools)" term.** Enabled MCP servers' tool schemas ride every model turn
  alongside the prompt, and ADR-0014 established that the meter must never lie about what a turn
  actually costs. `McpRegistry` keeps an in-memory, never-persisted `tools_bytes` display cache
  (server name → last-known serialized size of its tool definitions), populated by whichever event
  last listed the tools — a `/mcp/{name}/probe`, or a turn's own bridge connect. The meter sums this
  cache across *enabled* servers only; a server never yet listed contributes 0 (unknown is not
  invented as zero-cost, it's simply not counted yet). This is **stale-but-honest**: the cache can lag
  a server's actual current tool set by one probe/turn, which is preferable to a per-render network
  call blocking every page load on N servers' health.
- **The "known tools" disclosure.** `McpRegistry` keeps a second in-memory, never-persisted cache
  (`known_tools`: server name → last-known `Vec<ToolSummary>`, name + description only, never the
  JSON schema) alongside `tools_bytes`, same stale-but-honest posture and same population trigger (a
  probe). Unlike the probe *status* chip — which deliberately re-renders as "not probed" on every
  `GET /mcp` load, because a stale "ok" would misrepresent a server's current health — the tool-name
  list is a display convenience, not a health claim, so it is read back and shown even across a page
  refresh and even alongside a since-failed reprobe. `crate::mcp` still never imports `ai::mcp`
  (§ dependency rule below): `ToolSummary` is a local, minimal mirror of `ai::mcp::McpTool`, and
  `web::routes::mcp` does the conversion.

## Consequences

- The owner can point the foil at any number of MCP servers without a code change — add, disable,
  edit, or remove them from `/mcp` with no restart, matching the live-tunable posture
  [ADR-0011](./0011-live-switchable-llm-backend.md) already established for the backend/model/params.
- **New module `crate::mcp`** (config/persistence) and **new submodule `ai::mcp`** (wire client) join
  the module graph ([02-module-reference](../02-module-reference.md), D4/D5); the one-way
  `ai::backend → {ai::mcp, crate::mcp}` edge is a normative addition to the dependency rules table —
  neither of the two new modules may import the other or `ai::backend`.
- **`<vault>/.mcp-servers.json` sits inside the vault directory but outside the vault's truth/index
  contract** ([03-data-model](../03-data-model.md)) — a new kind of file the reindex invariant must
  keep ignoring, not by a special-case exclusion rule but structurally, because `vault::walk` only
  ever descends into idea directories.
- **Ollama turns gain a second reason to run the multi-round tool loop instead of a single-shot
  call** — `web_access` was previously the only trigger (ADR-0017); "at least one MCP server
  enabled" is now an equal-weight second one. Callers (chat/skill/swarm/workflow) are unaffected —
  they still see one `chat()` call in, one reply out.
- **Bearer tokens are a real secret at rest.** `.mcp-servers.json` is plaintext JSON on disk
  (matching how `CLAUDE_CODE_OAUTH_TOKEN` already lives in `.env` — no secret manager exists in this
  app's scope); the UI's write-only discipline (never echo a stored token) is the only mitigation
  against a shoulder-surfed screen, not against filesystem access. `.dockerignore`/backups should
  treat it like any other credential file.
- **A misbehaving MCP server degrades a turn, never fails it outright** — same D20 posture ADR-0017
  established for the web tools — but a server that is slow-but-not-timed-out on every turn is a real
  latency cost the owner absorbs by enabling it; there is no per-server timeout budget beyond
  `ai::mcp`'s fixed 3s/15s constants.
- **Outbound network reachability to arbitrary owner-chosen hosts** is now possible whenever any
  server is enabled — an extension of the same posture ADR-0017 already introduced for
  `IDEA_VAULT_SEARCH_URL`/DuckDuckGo/arbitrary fetch targets, noted in
  [12-deployment](../12-deployment.md).

## Alternatives considered

- **Store the registry in SQLite instead of a JSON file** — would put it alongside the other
  app-managed state, but SQLite is the *derived, rebuildable* index over the vault
  ([ADR-0002](./0002-markdown-source-of-truth-sqlite-index.md)); the reindex invariant requires
  everything in it to be reconstructable by re-scanning markdown, and there is no markdown source to
  rebuild an MCP server list from. Rejected: it would either violate the invariant or force a second,
  parallel "app config that isn't derived from anything" carve-out inside a table designed to hold
  only derived data.
- **Per-idea MCP configuration** (each idea's frontmatter lists its own enabled servers) — would let
  different ideas use different tool sets, but the owner's actual mental model is "the tools I have
  installed," not "the tools this one idea needs" — matching how `web_access` is one process-wide
  setting, not per-idea. Rejected: adds a frontmatter field and a per-idea UI for a distinction the
  owner didn't ask for; can be revisited if per-idea scoping is ever wanted.
- **Running probes as background jobs** (`web::jobs`, the same claim → spawn → poll shape as
  chat/skill/swarm) — would give probes the same navigate-away resilience real model turns need, but
  the job registry is keyed per-*idea* ([ADR-0010](./0010-ai-turns-as-background-jobs.md)) and an MCP
  server is idea-agnostic, owner-global infrastructure — there is no idea slug to key a probe's job
  slot on, and inventing one would be scope mismatch for a call already bounded to single-digit
  seconds by `ai::mcp`'s own timeouts. Rejected in favor of a plain inline `async` handler.

---

> ADRs are immutable once **Accepted**. To change this decision, write a new ADR that supersedes this
> one and update the Status line above.
