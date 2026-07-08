# 11 — Glossary

> Canonical vocabulary for idea-vault. Every other doc, and eventually the code, uses these terms
> with exactly these meanings. Where a term maps to a code identifier, the identifier is given
> verbatim so docs and code never drift.

## Core domain

- **Idea** — the unit of work. One idea = one folder in the vault. Modeled in code as
  `domain::idea::Idea`. Has exactly one **state** at any time.
- **Vault** — the on-disk directory (`vault/`) containing all ideas. The **source of truth** for
  everything the user reads. Never derived; always authoritative.
- **Slug** — the URL- and filesystem-safe identifier for an idea, derived from its title
  (`domain::slug`). Unique within the vault; collisions are suffix-disambiguated. Also the target of
  `[[slug]]` links. Example: `distributed-idea-market`.
- **Idea state** — one of exactly four values, represented by the `domain::idea::IdeaState` enum.
  The names are canonical and must match verbatim in docs and code:
  - **`Draft`** — just created; not yet interrogated.
  - **`InDiscussion`** — the active interrogation loop.
  - **`Stored`** — the user has finished; memory has been extracted; the idea is dormant but complete.
  - **`Reopened`** — a previously `Stored` idea brought back into discussion with its memory reloaded.
  - (Written in frontmatter in lower-kebab as `state: in_discussion`, etc. — see [03-data-model](./03-data-model.md).)

## Storage artifacts

- **`idea.md`** — per-idea file: YAML **frontmatter** (state, slug, timestamps, tags) plus the body,
  which holds the *current best statement* of the idea. Rewritten on Store.
- **`conversation.md`** — per-idea **append-only** transcript of the discussion (user and assistant
  turns). Never rewritten, only appended.
- **Memory fact** — one durable, distilled conclusion about an idea, stored as a single file in the
  idea's `memory/` directory. Modeled as `domain::memory::MemoryFact`. One fact per file.
- **`MEMORY.md`** — per-idea one-line index of the files in `memory/`, loaded as context when the
  idea is reopened. Mirrors the agent-harness memory-index convention.
- **Frontmatter** — the YAML block at the top of `idea.md` (and of each memory fact file) carrying
  structured, indexable fields. Parsed by `domain::frontmatter`.
- **`[[slug]]` link / backlink** — a cross-reference from one idea or memory fact to another idea by
  slug. Resolved on reindex into the `backlinks` index table. See [D23](./06-concepts/memory.md).
- **Artifact** — a persisted knowledge-extraction output stored under `vault/<slug>/artifacts/`.
  Modeled as `domain::artifact::Artifact` (frontmatter + body). `.md` is **truth**, indexed into
  `search_fts` as kind `'artifact'`; `.html` is a **derived, unindexed export** — a standalone,
  self-contained report the owner can open or share, not a knowledge source. See
  [ADR-0015](./adr/0015-knowledge-extraction-artifacts.md).

## Index

- **Index** — the **SQLite** database (`index.db`) holding search, tags, and backlink tables. It is
  **derived and rebuildable** — never a source of truth.
- **Reindex** — the operation that rebuilds the entire index by walking `vault/**` and re-parsing
  markdown (`index::reindex`). The **reindex invariant**: the index must always be fully
  reconstructable from the vault alone. See [ADR-0002](./adr/0002-markdown-source-of-truth-sqlite-index.md).
- **FTS5** — SQLite's full-text search extension, used for search over idea bodies and conversations.

## AI

- **Ollama** — the local LLM server (default `http://localhost:11434`) idea-vault talks to by
  default. See [ADR-0003](./adr/0003-ollama-local-only-ai.md).
- **claude-code backend** — the second, agentic LLM backend: idea-vault spawns the owner's local,
  authenticated `claude` CLI as a one-shot process per turn. Not a cloud API call. See
  [ADR-0009](./adr/0009-pluggable-llm-backend-claude-code.md).
- **`LlmBackend`** (`ai::backend::LlmBackend`) — the **live router** holding both backends plus
  runtime-tunable `LlmSettings`; every AI call re-reads the current settings to pick the active
  backend and its tuned parameters. Switchable via the Settings page (`GET`/`POST /settings`) with
  no restart. See [ADR-0011](./adr/0011-live-switchable-llm-backend.md).
- **Background job** (`web::jobs`) — the detached async task an AI-driven route (chat/skill/swarm)
  spawns to run the model call; one job per idea. The browser polls
  `GET /idea/:slug/pending` for a server-driven "thinking… Ns" indicator until it resolves. Replaced
  SSE token streaming. See [ADR-0010](./adr/0010-ai-turns-as-background-jobs.md).
- **SSE (Server-Sent Events)** — the one-way streaming channel originally used to push AI tokens to
  the browser token-by-token. **Superseded** by the background-job + poll model above; see
  [ADR-0004](./adr/0004-sse-token-streaming.md) (superseded) and
  [ADR-0010](./adr/0010-ai-turns-as-background-jobs.md) (current).
- **Context budget** — the bounded amount of text (idea body + selected memory + recent
  conversation) assembled into a prompt, kept within the model's limits by `ai::budget`.
- **Degradation** — the defined behavior when the active LLM backend is slow or absent: the UI
  surfaces a clear state and never hangs. See [D20](./05-ai-integration.md).

## Harness primitives (the first-class concepts)

- **Memory (concept)** — the feature of extracting facts on Store and reloading them on Reopen.
  Doc: [06-concepts/memory](./06-concepts/memory.md).
- **Skill** — a named, reusable ideation move (a parameterized prompt template) applied to an idea.
  Doc: [06-concepts/skills](./06-concepts/skills.md).
- **Agent** — a scoped subagent role (e.g. critic, researcher, synthesizer) with a specific prompt
  and I/O contract. Doc: [06-concepts/agents](./06-concepts/agents.md).
- **Workflow** — a deterministic, multi-step orchestration over an idea (fan-out → judge →
  synthesize). Contrast with free-form chat. Doc: [06-concepts/workflows](./06-concepts/workflows.md).
- **Swarm / swarming** — running many agents concurrently against one idea, under **bounded
  concurrency**, then converging their outputs. Doc: [06-concepts/swarm](./06-concepts/swarm.md).
- **Bounded concurrency** — the hard cap (a semaphore) on how many AI calls (to whichever backend is
  active) run at once during a swarm, protecting a single local machine. See
  [ADR-0006](./adr/0006-bounded-concurrency-swarm.md).
- **Converge / synthesize** — the final step of a swarm/workflow where multiple agent outputs are
  judged and merged into one result.

## System / code

- **Single crate** — idea-vault ships as one binary Cargo crate with strict internal modules; not a
  workspace (yet). See [ADR-0005](./adr/0005-single-crate-vs-workspace.md) and [02-module-reference](./02-module-reference.md).
- **AppState** — the shared, cloneable application state (config, index handle, the `LlmBackend`
  router, concurrency limiter, background job registry) held by axum handlers.
- **Askama** — the compile-time HTML templating library used for server-rendered pages.
- **HTMX** — the client-side library that turns HTML attributes into AJAX/polling interactions,
  avoiding a JS SPA.
- **Partial** — an HTML fragment (not a full page) returned to HTMX to swap into the DOM.

## Diagram vocabulary

- **Diagram ID (Dn)** — every diagram in the docs has a stable ID in `D1`…`D30`, catalogued in
  [08-diagrams](./08-diagrams.md). References elsewhere use the ID.
- **Home doc** — the single document a diagram is authored in; the registry only links to it.
