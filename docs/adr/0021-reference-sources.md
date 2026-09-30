# ADR-0021 — Reference sources: registry, generated compose override, deterministic tool leaves

- **Status:** Accepted — amended by [ADR-0034](./0034-grounded-ranked-and-bounded-workflow-stages.md) (the workflow Ground stage also reads attached sources, on a 2 × 2 tool budget)
- **Date:** 2026-07-16
- **Deciders:** owner

## Context

The foil can crawl the web ([ADR-0017](./0017-web-access-tools.md)) and call the owner's MCP
servers ([ADR-0018](./0018-mcp-servers.md)), but it cannot read the owner's own reference material
on disk — prior notes, an Obsidian vault, a sibling repo's docs — which is exactly the context that
sharpens the interrogation of an idea. The claude-code backend technically could
(`IDEA_VAULT_CLAUDE_ADD_DIRS`), but that knob is process-wide, frozen at boot, claude-only, and
meaningless in a container where the directory was never mounted. The Ollama path had nothing.

Containers are the hard constraint. A host path means nothing inside the container until a bind
mount exists; bind mounts only change on a `docker compose up`; and two standing decisions bound
the solution space: **the app never runs docker and nothing auto-starts** (`restart: "no"`, manual
bring-up — [ADR-0020](./0020-boot-order-and-ghost-binds.md)), and **a bind source is never
invented** (`create_host_path: false` — [ADR-0019](./0019-vault-mount-verified-not-created.md)).
Whatever mechanism gets a source into the container has to live with a human in the loop.

There is also a shape to respect on the model-facing side: handing a local model free-form
filesystem tools (arbitrary paths, regexes, argv) is both a prompt-injection surface and a recipe
for nondeterministic flailing. The sibling td-bot convention this repo's shipping gate also adopts
calls the alternative **DRT — deterministic tools**: the model supplies *intent*, code supplies
*mechanism*.

## Decision

We will add an owner-managed **reference-source registry** (`crate::sources::SourceRegistry`), a
**generated compose override** the owner applies by hand, **per-idea attachment** in frontmatter,
and **deterministic read-only tool leaves** (`ai::sources`) scoped to each turn.

- **Registry — a vault dotfile, no enabled flag.** `Vec<SourceConfig>` (`name` → absolute
  `host_path`) mirrored to `<vault>/.sources.json` (`IDEA_VAULT_SOURCES_CONFIG`), for the same
  reasons as ADR-0018's `.mcp-servers.json`: **app config, not vault truth**, riding the one
  host-persistent bind mount, structurally invisible to reindex (`vault::walk` only admits
  directories containing an `idea.md`), atomic tmp+rename persistence, missing/unparsable file
  degrades to an empty registry rather than a boot crash. Names share the slug alphabet
  (`[a-z0-9-]`) because they double as the container mount target (`/mnt/sources/<name>`) and the
  tool routing key; a name is **immutable** (remove + re-add to rename) because renaming would
  silently orphan every idea's frontmatter attach list. Unlike MCP servers there is deliberately
  **no `enabled` flag**: a registered source is inert until an idea attaches it — per-idea attach
  *is* the opt-in, and removal is the only off switch. A process-wide toggle would gate nothing an
  attach list doesn't already gate, and would add a second place a source can be "off".
- **Duplicate host paths are rejected**, on add and on edit — two names for one directory would
  alias the same content under two routing keys, and "which name did the model cite?" must stay
  answerable. The error names the existing entry.
- **The compose override is generated, never applied, by the app.** Every mutation regenerates
  `<vault>/.docker-compose.sources.yml` as a pure function of the registry (sorted, idempotent,
  golden-tested): one read-only bind per source at `/mnt/sources/<name>`, plus
  `IDEA_VAULT_SOURCES_APPLIED: <fingerprint>` baked into the `idea-vault` service environment. The
  **owner** applies it with `docker compose up -d` (a one-time `COMPOSE_FILE` entry in `.env` makes
  that the plain default command — see [12-deployment](../12-deployment.md), D31); the app never
  invokes docker and the ADR-0020 `restart: "no"` manual bring-up posture is unchanged. The gap
  between *saved* and *applied* is a first-class UI state: the fingerprint is deliberately
  human-debuggable (`name=path` pairs sorted by name, `;`-joined, no hashing — `docker inspect`
  shows exactly what was applied), and any entry not verbatim in the applied value renders as
  **NeedsReup**. Zero sources still renders the file (empty fingerprint, no `volumes:` key) so a
  `COMPOSE_FILE` that lists the override keeps working.
- **`create_host_path: false` on every generated bind, blast radius accepted.** A source host dir
  that vanished (deleted, renamed, unmounted) fails the **entire `up`** — the app container does
  not start until the owner fixes the path or removes the source. Accepted deliberately:
  never-invent outranks partial availability (ADR-0019's whole lesson is that a silently-invented
  empty directory is worse than a loud failure), and the Sources page's **Missing** pill pre-warns
  before the owner ever re-`up`s.
- **`Mounted { entries: 0 }` is a warning, not a green light.** ADR-0020 lineage: a bind whose
  host side went away *underneath a running container* lists as an empty directory instead of
  erroring — the ghost-bind signature. The status probe reports the direct child count and the UI
  renders zero as suspect.
- **Per-idea attach lives in frontmatter, and only in frontmatter.** `sources: [name]` on
  `IdeaFrontmatter` (`#[serde(default, skip_serializing_if)]`, so pre-feature ideas round-trip
  byte-identically). **Explicit non-goal for v1: no SQLite tables, no filter-ideas-by-source.**
  The index stays trivially rebuildable and `index::reindex` needs zero knowledge of sources; if
  filtering is ever wanted, the attach lists are already on disk to derive it from
  ([ADR-0002](./0002-markdown-source-of-truth-sqlite-index.md)).
- **The DRT tool contract.** The model picks **which** source — a JSON-schema *enum of the
  attached names* — and **what** to look for: a literal, case-insensitive query (`source_grep` is
  NOT a regex) or a relative path returned by an earlier leaf. Code decides **how**: the registry
  resolves the name to a canonical root (`resolve_attached` — `std::fs::canonicalize` has
  succeeded on every root it returns, and unknown/unmounted names are warn-dropped so a stale
  attachment degrades the turn, never kills it), and every relative path funnels through
  `resolve_rel`, the containment gate (reject absolute paths and `..` before touching the
  filesystem, canonicalize, require the result still under the canonical root — which is what
  makes a symlink inside a source that points outside it fail closed). **Never model-authored
  host paths, regexes, or argv.** Tool errors are content the model can route around, output is
  bounded per leaf, and walks are sort-ordered so the same query yields the same text (D20 +
  determinism, mirroring `ai::web`).
- **Per-turn scoping, not process state.** `LlmBackend::with_turn_sources(Vec<ResolvedSource>)`
  builds a per-job scoped clone; the web job layer resolves the idea's attach list at spawn time,
  so attaching/detaching is live on the very next turn and turns with no idea in scope stay
  source-free by construction. On the Ollama path the `source_*` leaves merge into the same
  bounded tool loop web access (ADR-0017) and MCP (ADR-0018) already run in — attached sources
  are a third equal-weight reason that loop runs. On the claude-code path each resolved root
  becomes an `--add-dir` plus a system-prompt note (the Ollama equivalent is a prompt prefix —
  never both).
- **Bare mode is inert.** `IDEA_VAULT_SOURCES_DIR` unset (or blank) means a bare `cargo run`:
  registry host paths are read directly, the override is still generated but nothing consumes it,
  and `NeedsReup` cannot occur (there is no applied fingerprint to disagree with). The compose
  base file — not the owner — sets `/mnt/sources`, which is also the container-mode signal.

## Consequences

- **New module `crate::sources`** (registry + override generation, `std`/`serde` only) and **new
  submodule `ai::sources`** (the DRT leaves) join the module graph; as with ADR-0018,
  `ai::backend` is the one place that combines "which sources this turn" with "how to read one" —
  `sources` never imports `ai`.
- **Applying a source change is a two-step act by design**: save on `/sources`, then
  `docker compose up -d`. The app cannot make a bind mount appear (ADR-0020), so the honest thing
  is to surface the gap (**NeedsReup**) rather than pretend liveness — the same
  measured-not-assumed posture ADR-0020 established for boot.
- **One vanished host dir stops the whole stack from coming up** until fixed or removed — the
  accepted price of never-invent. The failure is loud, named by compose, and pre-warned by the
  **Missing** pill; recovery is documented in [12-deployment](../12-deployment.md) pitfalls.
- **Host paths are machine-identifying**, and the two dotfiles ride a vault the owner may version
  and publish independently: the registry maintains `vault/.gitignore` (create-or-append, never
  rewrite owner content) covering both files, and writes them `0600`.
- **The reindex invariant is untouched.** Both dotfiles are structurally invisible to the walker,
  frontmatter is already indexed source material, and no derived table references sources —
  `reindex(V) == reindex(reindex(V))` holds without a new precondition.
- **Known follow-up — `num_ctx` is not re-floored after tool-result growth.** The Ollama window
  is floored at the *assembled* prompt before the tool loop starts, but tool results appended
  mid-loop (a `source_read` near its 12k-char cap, several grep rounds) grow the effective prompt
  past what the floor was sized for, and Ollama would silently truncate the head. Pre-existing
  and shared with the web (ADR-0017) and MCP (ADR-0018) tool results — sources make it more
  likely, not newly possible. Recorded here so it is fixed once for all three, not per-tool.

## Shipping discipline

This feature landed through `bash scripts/gate.sh` — the fixed-order no-mistakes gate adapted from
the td-bot DRT convention: intent (`docs/INTENT.md`, written before the code) → invariant greps
(`scripts/check-invariants.sh`, one grep per protected ADR) → build → tests → fmt →
`clippy -D warnings`, stopping on the first red. The gate is documented in the script header, in
CLAUDE.md's commands, and here — **deliberately not as its own ADR**: it is shipping process, not
architecture, and an ADR's immutability contract is the wrong home for a checklist that should
evolve freely with the codebase.

## Alternatives considered

- **An `enabled` flag on the registry, like MCP servers** — rejected: MCP's toggle gates tools
  that ride *every* turn process-wide, so "installed but off" is meaningful there; a source only
  ever reaches the model through an idea's attach list, so a toggle would duplicate the attach
  list's job and create a second, disagreeing off switch. Removal is the only off.
- **SQLite tables + filter-ideas-by-source** — rejected for v1: every derived row must be
  reconstructable from markdown (ADR-0002), which is satisfiable (frontmatter holds the attach
  lists) but buys a filter nobody asked for at the cost of schema, reindex, and drift surface.
  Revisit when a real filtering need shows up.
- **The app applies the override itself (`docker compose up -d` on save)** — rejected: ADR-0020
  decided the app never runs docker and nothing auto-starts, for measured reasons; it would also
  need the docker socket in the container (root-equivalent), the exact watchdog-container shape
  ADR-0020 already rejected.
- **Free-form filesystem tools (model-supplied paths/regex/argv, or a generic shell leaf)** —
  rejected: DRT is the point. A model-authored path is a containment bug waiting for a clever
  prompt; a model-authored regex is a ReDoS and a nondeterminism source. The model picks which
  source and a literal query; code does everything else.
- **Reuse `IDEA_VAULT_CLAUDE_ADD_DIRS` and just document it** — rejected: process-wide (every
  idea sees every dir), boot-frozen, claude-code-only, and dead in containers without the very
  mount machinery this ADR adds. It stays as the global fallback underneath per-idea sources.
- **`create_host_path: true` on generated binds so a vanished dir can't fail `up`** — rejected:
  the daemon would invent an empty root-owned directory and the model would silently see an empty
  source — the precise ghost class ADR-0019/0020 exist to prevent. Loud beats wrong.

## Related

- [ADR-0002](./0002-markdown-source-of-truth-sqlite-index.md) — why the attach list is frontmatter
  and the index gains no source tables
- [ADR-0017](./0017-web-access-tools.md) / [ADR-0018](./0018-mcp-servers.md) — the tool loop the
  DRT leaves merge into; errors-as-content discipline
- [ADR-0019](./0019-vault-mount-verified-not-created.md) /
  [ADR-0020](./0020-boot-order-and-ghost-binds.md) — never-invent, ghost binds, the app never runs
  docker
- [docs/12-deployment.md](../12-deployment.md) — D31 topology, config rows, `COMPOSE_FILE` setup,
  pitfalls

---

> ADRs are immutable once **Accepted**. To change this decision, write a new ADR that supersedes this
> one and update the Status line above.
