# 07 — Runtime Flows

> A narrative index of every runtime flow, each pointing at its sequence/activity diagram in the doc
> that owns it. This document **authors D10** (new-idea creation); the other flow diagrams live with
> their subsystem so they sit next to the code concepts they describe (no duplication — see
> [08-diagrams](./08-diagrams.md)).

## The flow map

| # | Flow | Diagram | Authored in |
|---|------|---------|-------------|
| 1 | Startup / boot | D25 | [01-architecture](./01-architecture.md) |
| 2 | New-idea creation | **D10** | *this doc* |
| 3 | Chat turn → Ollama → SSE stream | D11 | [05-ai-integration](./05-ai-integration.md) |
| 4 | Store → memory extraction | D12 | [06-concepts/memory](./06-concepts/memory.md) |
| 5 | Reopen → load memory | D13 | [06-concepts/memory](./06-concepts/memory.md) |
| 6 | Subagent swarm (fan-out → converge) | D14 | [06-concepts/swarm](./06-concepts/swarm.md) |
| 7 | Skill invocation | D18 | [06-concepts/skills](./06-concepts/skills.md) |
| 8 | Workflow orchestration | D19 | [06-concepts/workflows](./06-concepts/workflows.md) |
| 9 | Reindex (rebuild from markdown) | D15 | [03-data-model](./03-data-model.md) |
| 10 | HTTP request / middleware lifecycle | D16 | [09-web-ui](./09-web-ui.md) |

These ten cover every flow named in [CLAUDE.md](../CLAUDE.md); the six *core* flows it calls out
explicitly are #2, #3, #4, #5, #6, #9.

## The idea lifecycle, end to end (prose)

1. **Create (D10).** Owner enters a title; a `Draft` idea folder is created.
2. **Discuss (D11, D18, D14).** Owner submits turns; the AI streams replies. The owner can invoke
   skills or launch a swarm to attack the idea from many angles. State is `InDiscussion`.
3. **Store (D12).** Owner says "store it". The idea body is consolidated and memory facts are
   extracted. State → `Stored`.
4. **Reopen (D13).** Later, the owner reopens; memory is reloaded as context and discussion resumes.
   State → `Reopened`, and Store can happen again (merging memory).

Underlying all of it: writes go to markdown first, then the SQLite index; the index can always be
rebuilt from the vault (D15).

## D10 — New-idea creation

```mermaid
sequenceDiagram
    autonumber
    participant B as Browser (HTMX)
    participant H as web::routes::ideas (create)
    participant Slug as domain::slug
    participant V as vault::store
    participant Idx as index

    B->>H: POST /ideas (title, optional seed body)
    H->>H: validate title non-empty
    H->>Slug: slugify(title) + collision check (D22)
    Slug-->>H: unique slug
    H->>V: create vault/<slug>/
    H->>V: write idea.md (frontmatter state=draft, title, slug, timestamps)
    Note over V: conversation.md created empty; no memory/ yet (Draft has no memory)
    H->>Idx: upsert ideas row (+ tags if provided)
    H-->>B: 200 → HTMX redirect/swap to /idea/<slug> (Draft view)
```

Post-conditions: a `Draft` idea exists on disk and in the index; the owner lands on the idea page
ready to submit the first turn (which transitions it to `InDiscussion`, [D9](./04-state-machine.md)).

## Related

- [04-state-machine](./04-state-machine.md) — the states these flows move between.
- [09-web-ui](./09-web-ui.md) — the routes (D17) and middleware (D16) these flows enter through.
