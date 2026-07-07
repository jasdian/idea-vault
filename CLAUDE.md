# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Status: scaffolded — skeleton built, features are stubs

The crate skeleton exists and matches the docs: `cargo run` boots the D25 sequence and serves
`GET /` and `GET /admin/health`; `domain/` is fully implemented and unit-tested; `vault`, `index`
(real schema + list query), `ai` (real client + probe), `memory`, `concepts`, and most `web`
routes are typed stubs returning **501 Not Implemented** with `TODO(<milestone>)` markers citing
their design docs. The "Commands" section below is real. Do not invent build/test output — run
the commands and report real results. Next build-out order: `vault` → `index` (reindex) →
`ai`/`memory` → `concepts` → `web` (chat SSE).

## What this is

**idea-vault** is a localhost web tool for solo ideation. The owner brings a raw idea, then
"runs it into the ground" in conversation with an AI — pushing it through every stage of thought,
however ridiculous — and when finished, tells the AI to **store the idea in the vault** so it can
be reopened and continued later.

It is deliberately modeled on how an LLM agent harness works: the same primitives — **memory**,
**skills**, **agents**, **workflows**, and **subagent swarming** — are first-class product
concepts here, applied to interrogating one idea rather than editing a codebase.

## Confirmed architecture decisions

These were chosen explicitly by the owner. Do not silently revise them; if you believe one is
wrong, raise it rather than working around it.

- **Backend / UI:** Rust, **axum** HTTP server. Server-rendered HTML via **Askama** templates,
  enhanced with **HTMX**; AI responses stream token-by-token over **Server-Sent Events (SSE)**.
  No JS build step, no SPA — ships as a single binary. New chat routes should stream, not block.
- **Storage: hybrid.** Markdown files on disk are the **source of truth**; **SQLite is a
  rebuildable index** for search/tags/backlinks only. Never store canonical idea content only in
  SQLite — anything in the DB must be reconstructable by re-scanning the vault. A `reindex`
  path that rebuilds the DB from markdown must always exist and stay correct.
- **AI backend: local models via Ollama** (`http://localhost:11434`). Fully offline/private.
  The subagent-swarm and skills concepts run against local models — keep prompt/context budgets
  modest and degrade gracefully when a model is slow or unavailable.
- No cloud AI provider is in scope. Do not add Anthropic/OpenAI calls without the owner asking.

## The ideation lifecycle (the core product loop)

An idea moves through explicit states — this state machine is the heart of the app. The canonical
names (the `IdeaState` enum, used verbatim in code and docs) are in parentheses; see
[docs/04-state-machine.md](docs/04-state-machine.md) for the full transition table:

1. **Draft** (`Draft`) — owner enters a new idea; a vault folder is created.
2. **In discussion** (`InDiscussion`) — the working loop. Owner and AI converse; the AI is expected
   to be a rigorous foil: steelman, then stress-test, from many angles. This is where "run it into
   the ground" happens and where subagent swarming is triggered.
3. **Stored** (`Stored`) — owner signals they're done ("store it in the vault"). The AI produces a
   consolidated writeup and extracts **memory** (durable facts/decisions) for the idea. The idea
   is now dormant but complete.
4. **Reopened** (`Reopened`) — a stored idea can re-enter discussion later, carrying its memory
   forward as context, exactly like an LLM resuming with prior memory loaded.

The state must be persisted in the idea's markdown frontmatter, not only in SQLite.

## Design docs (build against these)

The full design foundation now lives in [`docs/`](docs/README.md): architecture (C4), the
single-crate module graph, the vault/SQLite data model, the lifecycle state machine, AI/Ollama
integration, the five harness concepts (memory/skills/agents/workflows/swarm), the web-UI routes,
a 25-diagram Mermaid catalog, and ADRs 0001–0007. Start at [docs/README.md](docs/README.md).
The code is not scaffolded yet; these docs are the contract to build against.

## Vault layout (source of truth)

Each idea is a folder of markdown. Proposed shape (finalize during scaffolding):

```
vault/
  <idea-slug>/
    idea.md          # frontmatter (state, created, tags) + the current best statement of the idea
    conversation.md  # append-only transcript of the discussion
    memory/          # one durable fact/decision per file, LLM-memory style
      *.md           # frontmatter + body; link related memories with [[slug]]
    MEMORY.md        # one-line index of memory/ files, loaded as context when the idea reopens
index.db             # SQLite: rebuildable search/tag/backlink index over vault/**
```

The `memory/` + `MEMORY.md` convention intentionally mirrors an LLM agent's file-based memory:
small single-fact files, a loaded index, `[[slug]]` cross-links.

## LLM-inspired concepts → how they map here

When implementing these, keep the mental model close to a real agent harness:

- **Memory** — per-idea durable facts extracted at "store" time and reloaded on reopen. Files,
  not a monolith. This is what makes an idea resumable.
- **Skills** — reusable, named ideation moves the AI can apply to an idea (e.g. "premortem",
  "find the cheapest disproof", "market-size it", "devil's advocate"). Loadable/composable.
- **Agents** — specialized subagent roles (critic, researcher, synthesizer) with scoped prompts.
- **Workflows** — deterministic multi-step orchestrations over an idea (fan-out → judge →
  synthesize), as opposed to free-form chat.
- **Subagent swarming** — fan out N agents in parallel to attack one idea from independent angles,
  then converge/synthesize. Against local Ollama models this means bounded concurrency and careful
  context budgeting — do not naively spawn unbounded parallel calls.

## Intended commands (once scaffolded)

Standard Cargo — reflect the real state of the repo, don't fabricate:

```bash
cargo run                 # start the localhost web UI
cargo build --release     # single-binary release build
cargo test                # run tests
cargo test <name>         # run a single test by name substring
cargo fmt && cargo clippy # format + lint before finishing a change
```

Ollama must be running locally (`ollama serve`, model pulled) for AI features to work; the app
should detect its absence and surface a clear UI state rather than hanging.

### Containers (the primary way to host it)

The whole stack (app + Ollama) runs in containers, locally, with or without a GPU. The
`Dockerfile` and Compose files already exist as the deployment contract (they build once the crate
is scaffolded). Full topology, build pipeline, and pitfalls are in
[docs/12-deployment.md](docs/12-deployment.md) ([ADR-0008](docs/adr/0008-containerized-local-deployment.md)).

```bash
cp .env.example .env                                                   # set IDEA_VAULT_UID/GID to your id -u / id -g
docker compose up -d --build                                           # CPU mode (default)
docker compose -f docker-compose.yml -f docker-compose.gpu.yml up -d   # GPU mode (nvidia; needs nvidia-container-toolkit)
docker compose --profile tools run --rm ollama-pull                    # first run: pull the model
# open http://localhost:3000
docker compose down
```

**Config is env-driven, not hardcoded.** `config.rs` reads `IDEA_VAULT_BIND` (default
`127.0.0.1:3000`; `0.0.0.0:3000` in-container), `IDEA_VAULT_VAULT_DIR`, `IDEA_VAULT_INDEX_PATH`,
`IDEA_VAULT_OLLAMA_URL` (default `http://localhost:11434`; `http://ollama:11434` in-compose),
`IDEA_VAULT_OLLAMA_MODEL`, `IDEA_VAULT_AI_CONCURRENCY` (default `2`, the shared Ollama-call bound K,
ADR-0006), and `IDEA_VAULT_OLLAMA_TIMEOUT_SECS` (default `120`, the hard inactivity timeout).
**Never hardcode `localhost:11434` or a localhost bind** — it breaks the
containerized run. `vault/` is a host bind mount (truth you own); the SQLite index and Ollama models
are named volumes (rebuildable / re-pullable). GPU touches only the Ollama service.

## Conventions

- Markdown is the durable format everywhere the owner reads content; render it in the UI.
- Frontmatter carries structured state (idea state, tags, timestamps) so the vault is
  self-describing and the SQLite index is always regenerable from disk.
- Prefer streaming (SSE) for anything AI-generated so long local-model responses feel live.
- Keep it a single self-contained binary + a `vault/` directory the owner can read, back up,
  and version with git independently of the app.
