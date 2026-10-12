# ADR-0044 — MCP `list_sources`, `register_source` and `attach_sources`

- **Status:** Accepted
- **Date:** 2026-10-12
- **Deciders:** Owner
- **Amends:** [ADR-0024](./0024-mcp-server-inbound.md) — the tool catalog grows from 17 to 20 tools.
  [ADR-0029](./0029-mcp-moves-and-full-idea-read.md) and
  [ADR-0036](./0036-mcp-list-workflows-and-run-workflow.md) — source management is no longer
  web-only.
- **Extends:** [ADR-0021](./0021-reference-sources.md).

## Context

An agent session drives idea-vault over the inbound MCP server
([docs/12](../12-deployment.md) "Agent sessions"). Grounding an idea in a repository, or in a
Graphify map of one ([docs/15](../15-obsidian-and-graphify.md)), means registering the directory as
a reference source and attaching it to the idea (ADR-0021). Both steps existed only as HTMX form
routes (R37 `POST /sources/add`, R43 `POST /idea/{slug}/sources`). So a session had to leave MCP
and post forms with curl, and the R43 editor *replaces* the whole set, so a session that posted
only the name it wanted silently detached everything else. The workflow book's Ground stage
(ADR-0034) and `list_workflows`' `needs_sources` flag point a client at sources it had no MCP way
to reach. The owner took the gap as an API change to make.

## Decision

We will add three synchronous tools (task support `Forbidden`, no model call, no replay cache).

1. **`list_sources`** (no arguments): the registry in registry order, each entry
   `{name, path, status, entries?, hint?}`. `status` is a fixed word, not the Sources page's prose:
   `readable` (a bare-mode host path), `mounted` (a container bind; `entries: 0` is the ADR-0020
   ghost-bind signal), `needs_reup` (the owner must run `docker compose up -d`), or `missing`.
2. **`register_source`** (`name`, `path`): R37's validation through the same
   `SourceRegistry::add` (the name alphabet; an absolute, `..`-free, non-root path; no duplicate
   name or path), plus one refusal R37 does not make: a path that overlaps the vault, meaning it is
   the vault, lies inside it, or contains it. The check compares the paths made absolute against
   the working directory (the default vault dir is the relative `./vault`), and again resolved
   through each one's longest existing ancestor (so a symlink, or a directory not created yet,
   cannot slip past). A path it cannot make absolute is refused. An overlapping source would hand
   the foil every idea's notes and run journals (ADR-0037). In an agent session the vault sits
   inside the checkout, so the checkout root itself is refused; register a subdirectory, or a
   checkout that holds no vault. The result is the new entry with its status. Every refusal is
   `invalid_params`.
3. **`attach_sources`** (`slug`, optional `attach`, optional `detach`, at least one non-empty):
   a **merge**, not R43's replace. The new set is the current frontmatter set without `detach`,
   followed by each `attach` name not already present, in order; a name in both lists ends
   attached. It goes through R43's own write path (`ideas::update_idea_sources`): the per-idea
   job-slot claim, the name checks (a newly added name must be registered; an already-attached
   name the registry no longer knows may stay), the frontmatter write and the reindex. The result
   is `{slug, sources: [{name, status, …}]}`; a name the registry no longer knows reads
   `not_registered`. A busy idea, an unknown idea or an unregistered name is `invalid_params`.
4. **Editing and removing a registered source stay web-only** (R40, R41). A removal can orphan
   other ideas' attachments, and a path edit re-points every idea that attached the name. Both are
   the owner's calls on `/sources`.
5. **R43 changes in one respect:** an unchanged submission writes nothing and skips the reindex,
   so `updated` is no longer bumped by a no-op save. That is the shared write path's rule, and it
   makes a repeated `attach_sources` idempotent from the vault itself.

## Consequences

- Easier: a session grounds an idea in a repository or its map entirely over MCP, and cannot
  detach a source by naming only the one it wanted.
- Harder: an MCP token now lets its holder point the foil at any host directory the app's user
  can read, as the localhost `/sources` form already did. The foil still reads sources only
  through the ADR-0021 tools and the ADR-0039 lockdown, and never over a path that overlaps the vault.
- Invariants: frontmatter `sources:` stays the truth; MCP adds no second write path for it, and
  the app still never runs docker (ADR-0020). A container registration reads `needs_reup` until
  the owner re-ups.
- Still web-only: extract, compact, tags, fork, rename, delete, and source edit and removal.

## Alternatives considered

- **Replace semantics for `attach_sources`, matching R43:** rejected. A client sees the idea's
  set only through `get_idea`, and a stale read would detach a source the owner attached since.
- **Separate `attach_sources` and `detach_sources` tools:** rejected. One merge call covers both,
  and a swap (detach one, attach another) stays a single frontmatter write.
- **`update_source` and `remove_source` tools:** deferred. See decision 4.
- **Leave it to curl on the form routes** ([docs/15](../15-obsidian-and-graphify.md)): rejected.
  It leaves MCP for a step the Ground stage depends on, and R43's replace semantics are a trap
  for a client.

---

> ADRs are immutable once **Accepted**. To change a decision, write a new ADR that supersedes this
> one and update the Status line above.
