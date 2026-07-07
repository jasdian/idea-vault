# 04 — Idea Lifecycle State Machine

> The core behavioral model: how an idea moves through its four states, what triggers each
> transition, what guards it, and what side effects fire. Home of **D9**.
> State is canonical in `idea.md` frontmatter ([ADR-0007](./adr/0007-state-in-frontmatter-not-db.md));
> the enum is `domain::idea::IdeaState`.

## The four states

| State (`IdeaState`) | frontmatter | Meaning |
|---------------------|-------------|---------|
| `Draft` | `draft` | Just created. Title + optional seed body. Not yet interrogated. |
| `InDiscussion` | `in_discussion` | The active loop — conversing with the AI, running the idea into the ground; swarms run here. |
| `Stored` | `stored` | Owner declared done. Consolidated writeup written; memory extracted. Dormant but complete. |
| `Reopened` | `reopened` | A `Stored` idea brought back into discussion with its memory reloaded as context. |

## D9 — State diagram

```mermaid
stateDiagram-v2
    [*] --> Draft: create idea (D10)

    Draft --> InDiscussion: first turn submitted
    Draft --> Draft: edit title/seed body

    InDiscussion --> InDiscussion: chat turn (D11) / run skill (D18) / swarm (D14)
    InDiscussion --> Stored: "store it" (D12 extract memory)

    Stored --> Reopened: reopen (D13 load memory)
    Stored --> [*]: (dormant — remains on disk)

    Reopened --> Reopened: chat turn / skill / swarm
    Reopened --> Stored: "store it" (D12 re-extract / merge memory)

    note right of Stored
        Side effects on entry:
        - consolidate best statement into idea.md body
        - extract memory/*.md facts
        - rebuild MEMORY.md
        - frontmatter state=stored, then index upsert
    end note

    note right of Reopened
        Side effects on entry:
        - read MEMORY.md + selected facts
        - assemble context under budget (D21)
        - frontmatter state=reopened
    end note
```

## Transitions (normative table)

| From | To | Trigger | Guard | Side effects (in order) |
|------|----|---------|-------|-------------------------|
| — | `Draft` | Create idea | title non-empty; slug free ([D22](./03-data-model.md)) | create `vault/<slug>/`, write `idea.md` (`state=draft`), index upsert |
| `Draft` | `Draft` | Edit title/seed | slug unchanged | rewrite `idea.md` body/`title`, bump `updated` |
| `Draft` | `InDiscussion` | First chat turn | Ollama reachable *or* degraded-notice shown ([D20](./05-ai-integration.md)) | append to `conversation.md`, set `state=in_discussion`, stream reply ([D11](./05-ai-integration.md)) |
| `InDiscussion` | `InDiscussion` | Chat turn / skill / swarm | — | append transcript; skills [D18], swarm [D14] |
| `InDiscussion` | `Stored` | "Store it" | at least one turn exists | consolidate `idea.md` body, extract `memory/*.md` ([D12](./06-concepts/memory.md)), rebuild `MEMORY.md`, `state=stored`, index upsert |
| `Stored` | `Reopened` | Reopen | idea exists | load `MEMORY.md`+facts, budget context ([D21](./06-concepts/swarm.md)), `state=reopened` ([D13](./06-concepts/memory.md)) |
| `Reopened` | `Reopened` | Chat turn / skill / swarm | — | append transcript |
| `Reopened` | `Stored` | "Store it" | — | re-consolidate, **merge** new facts into existing `memory/` (dedupe), rebuild `MEMORY.md`, `state=stored` |

## Invariants

- **State is persisted before it is trusted.** A transition is not complete until `idea.md`
  frontmatter is written; the index copy is a best-effort derivative ([ADR-0007](./adr/0007-state-in-frontmatter-not-db.md)).
- **`conversation.md` is append-only** across every discussion state; storing never truncates it.
- **Memory only grows or merges.** Re-storing a `Reopened` idea merges/dedupes facts; it does not
  silently drop prior conclusions.
- **`Draft` has no memory.** `memory/` and `MEMORY.md` first appear on the transition to `Stored`.
- **Reopening is idempotent w.r.t. truth.** It reloads context and flips state; it does not rewrite
  the body or memory (those change only on Store).

## Mapping to code

- Enum + serialized mapping: `domain::idea::IdeaState` and [03-data-model](./03-data-model.md) D8.
- Transition side effects on Store/Reopen live in `memory::extract` / `memory::load`
  ([06-concepts/memory](./06-concepts/memory.md)).
- The web handlers that drive transitions are in `web::routes` ([09-web-ui](./09-web-ui.md), D17).

## Related

- [00-vision](./00-vision.md) — why the loop is shaped this way ("run it into the ground").
- [06-concepts/memory](./06-concepts/memory.md) — D12/D13 detail the Store/Reopen side effects.
