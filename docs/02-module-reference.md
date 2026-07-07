# 02 — Module Reference

> The single-crate decomposition the code is built against, plus the enforced dependency rules.
> Home of **D4** (module dependency graph) and **D5** (module layout). Decision rationale:
> [ADR-0005](./adr/0005-single-crate-vs-workspace.md).

idea-vault is **one binary crate** (`idea-vault`) with strict internal modules. Boundaries are a
convention enforced by review and by the D4 rules — not by the compiler — but the layout is designed
so a future workspace split is mechanical.

## D5 — Module / file layout

```mermaid
flowchart TB
    subgraph crate["crate: idea-vault"]
        MAIN["main.rs — bootstrap (D25)"]
        APP["app.rs — router, AppState, middleware"]
        CFG["config.rs — paths, Ollama URL, limits (IDEA_VAULT_* env, D26)"]

        subgraph domain["domain/ (pure, no IO)"]
            D_IDEA["idea.rs — Idea, IdeaState"]
            D_MEM["memory.rs — MemoryFact, MemoryIndex"]
            D_FM["frontmatter.rs — parse/emit YAML"]
            D_SLUG["slug.rs — slug + collisions (D22)"]
        end

        subgraph vault["vault/ (disk = truth)"]
            V_STORE["store.rs — read/write idea.md, conversation.md, memory/*.md, MEMORY.md"]
            V_WALK["walk.rs — scan vault/** for reindex"]
        end

        subgraph index["index/ (SQLite = derived)"]
            I_SCHEMA["schema.rs — DDL + FTS5 (D6)"]
            I_QUERY["queries.rs — search, tags, backlinks"]
            I_REIDX["reindex.rs — rebuild-from-disk (D15)"]
        end

        subgraph ai["ai/ (Ollama boundary)"]
            A_OLL["ollama.rs — client + health (D20)"]
            A_STREAM["stream.rs — tokens → SSE (D11)"]
            A_BUDGET["budget.rs — context budgeting (D21)"]
        end

        subgraph memory["memory/ (feature)"]
            M_EXTRACT["extract.rs — conv → facts on Store (D12)"]
            M_LOAD["load.rs — facts → context on Reopen (D13)"]
            M_BACK["backlinks.rs — [[slug]] resolve (D23)"]
        end

        subgraph concepts["concepts/ (harness primitives)"]
            C_SKILL["skills.rs — registry + invoke (D18)"]
            C_AGENT["agents.rs — role prompts + I/O"]
            C_WF["workflows.rs — deterministic DAG (D19)"]
            C_SWARM["swarm.rs — bounded fan-out/converge (D14, D21)"]
        end

        subgraph web["web/ (HTTP surface)"]
            W_ROUTES["routes/ — ideas, chat, memory, admin"]
            W_SSE["sse.rs — shared SSE plumbing"]
            W_TMPL["templates.rs — Askama structs"]
        end

        TEMPLATES["templates/*.html — Askama sources"]
    end

    MAIN --> APP --> web
    W_TMPL -.renders.-> TEMPLATES
```

## D4 — Module dependency graph (allowed direction)

The single most important structural invariant: dependencies point **downward**, and **nothing
depends on `web`**. A violation (e.g. `domain` importing `web`, or `vault` importing `index`) is a
design smell caught in review.

```mermaid
flowchart TD
    web["web"]
    concepts["concepts"]
    memory["memory"]
    index["index"]
    ai["ai"]
    vault["vault"]
    domain["domain"]

    web --> concepts
    web --> memory
    web --> index
    web --> ai
    web --> vault
    web --> domain

    concepts --> ai
    concepts --> vault
    concepts --> domain

    memory --> ai
    memory --> vault
    memory --> index
    memory --> domain

    index --> vault
    index --> domain

    ai --> domain
    vault --> domain

    classDef top fill:#1f6feb22,stroke:#1f6feb;
    classDef base fill:#2ea04322,stroke:#2ea043;
    class web top;
    class domain base;
```

### Dependency rules (normative)

| Module | May depend on | Must **not** depend on |
|--------|---------------|------------------------|
| `domain` | (std/serde only) | anything internal |
| `vault` | `domain` | `index`, `ai`, `memory`, `concepts`, `web` |
| `ai` | `domain` | `vault`, `index`, `memory`, `concepts`, `web` |
| `index` | `vault`, `domain` | `ai`, `memory`, `concepts`, `web` |
| `memory` | `vault`, `ai`, `index`, `domain` | `concepts`, `web` |
| `concepts` | `ai`, `vault`, `domain` (read `index` via `memory` where needed) | `web` |
| `web` | everything below | (nothing may depend on `web`) |

> Rationale for a couple of edges that might surprise: `index` depends on `vault` because reindex
> reads markdown to rebuild ([ADR-0002](./adr/0002-markdown-source-of-truth-sqlite-index.md)). `ai`
> deliberately does **not** depend on `vault` — it is a pure model boundary; callers assemble prompts
> and hand them in.

## Module responsibilities

- **`domain`** — the vocabulary from [11-glossary](./11-glossary.md) as pure types: `Idea`,
  `IdeaState` (`Draft`/`InDiscussion`/`Stored`/`Reopened`), `MemoryFact`, frontmatter (de)serialize,
  slug rules. No IO, trivially testable.
- **`vault`** — the only module that reads/writes the markdown files; owns the on-disk file contract
  from [03-data-model](./03-data-model.md). Append-only for `conversation.md`.
- **`index`** — owns `index.db`: schema + FTS5, query functions, and `reindex` (the rebuild-from-disk
  that upholds the reindex invariant, [D15](./03-data-model.md)).
- **`ai`** — the sole Ollama boundary: HTTP client, health probe, token-stream→SSE adapter, and
  context budgeting. Provider-swap would be localized here (out of scope, [ADR-0003](./adr/0003-ollama-local-only-ai.md)).
- **`memory`** — the memory feature: extract facts at Store ([D12](./06-concepts/memory.md)), load
  them at Reopen ([D13](./06-concepts/memory.md)), resolve backlinks ([D23](./06-concepts/memory.md)).
- **`concepts`** — skills, agents, workflows, and the swarm orchestrator ([06-concepts](./06-concepts/)).
- **`web`** — axum router, handlers, Askama rendering, SSE plumbing. The top of the graph.

## Future workspace mapping (not built now)

If promoted to a workspace ([ADR-0005](./adr/0005-single-crate-vs-workspace.md)):

| Future crate | Absorbs modules |
|--------------|-----------------|
| `idea-vault-core` | `domain`, `vault`, `index` |
| `idea-vault-ai` | `ai`, `memory`, `concepts` |
| `idea-vault-web` | `web` + binary (`main`, `app`, `config`) |

The D4 direction already matches these crate boundaries, so extraction requires no dependency
inversion.
