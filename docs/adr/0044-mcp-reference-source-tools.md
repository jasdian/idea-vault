# ADR-0044 — MCP `list_sources`, `register_source` and `attach_sources`

- **Status:** Accepted
- **Date:** 2026-10-12
- **Deciders:** Owner
- **Amends:** [ADR-0024](./0024-mcp-server-inbound.md). The tool catalog grows from 17 to 20 tools,
  and "sources-management" leaves docs/13's Deferred list, except editing and removing a source.
- **Extends:** [ADR-0021](./0021-reference-sources.md) and
  [ADR-0043](./0043-obsidian-and-graphify-as-derived-layers.md) (docs/15 Layers 2 and 3).

## Context

An agent session drives idea-vault over the inbound MCP server (docs/12 "Agent sessions"). The
docs/15 Graphify flow needs two reference sources on an idea: the codebase, and the map built beside
it. MCP had no source tools, so a session had to `curl` the HTML form routes `POST /sources/add`
and `POST /idea/{slug}/sources` on loopback, and parse HTMX fragments to learn whether that worked.
The first real run of the flow, mapping JustVugg/colibri, hit exactly this gap.

## Decision

We will add three **synchronous** tools. None starts a job or makes a model call, so none takes a
task or an `idempotency_key`.

- **`list_sources`** returns the Sources page's row model: `name`, `path`, `status` (`mounted` with
  `entries`, `needs_reup` or `missing`).
- **`register_source {name, path}`** calls `SourceRegistry::add`, the function `POST /sources/add`
  uses, so the same checks apply:
  - the name alphabet (`Name`);
  - an absolute path with no `..`, newline or NUL, that is not the filesystem root;
  - no duplicate name or path.

  A refusal is a tool error carrying the registry's reason. The reply has `readable`, so a client
  learns at once when container mode needs the owner to re-`up` (ADR-0021).
- **`attach_sources {slug, sources}`** replaces the idea's attached set through `set_sources_core`.
  `POST /idea/{slug}/sources` now calls the same function, so there is one sequence:
  - it claims the per-idea job slot (refused while a run is in progress);
  - it checks every name;
  - a newly attached name must be registered, while an already-attached unknown name may stay;
  - it writes the frontmatter (truth), then reindexes.

  Duplicates collapse, order is kept, and `[]` detaches all.

Editing a source's path and removing a source stay off MCP. They change what other ideas' turns can
read, and they are the owner's actions on the Sources page.

## Consequences

- **Easier:** The docs/15 Layer 3 flow runs end to end over MCP:
  1. `graphify-map.sh`
  2. `register_source` ×2
  3. `attach_sources`
  4. `chat`/`run_workflow`

  `session-mcp.py` needs no change.
- **Harder:** An MCP token holder can now point the foil at any absolute host directory the app's
  user can read. The Sources page already gives that power to anyone who can reach the web UI, and
  the token is the stronger gate. Registration doesn't widen the foil by itself: a source is read only
  by turns on an idea that attaches it, under the ADR-0039 lockdown (`source_*` leaves or
  `--add-dir`).
- **Invariants kept:**
  - Frontmatter is the truth for attachment.
  - The registry stays app config, not vault content.
  - The HTTP and MCP paths share one core, so they can't drift.
  - No route, config key or schema change.

## Alternatives considered

- **Keep the curl recipe in docs/15.** Rejected. It couples agent sessions to HTMX fragment HTML,
  and it reports a refusal only as a 400 body.
- **One `manage_sources` tool with an action field**, including update and remove. Rejected. It
  puts destructive registry edits on MCP, and a single schema can't say which fields each action
  needs.
- **Attach implicitly when registering** (`register_source {…, attach_to: slug}`). Rejected. It
  mixes app config with idea truth in one call, and makes a partial failure ambiguous.

---

> ADRs are immutable once **Accepted**. To change a decision, write a new ADR that supersedes this
> one and update the Status line above.
