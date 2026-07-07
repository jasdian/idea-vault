# ADR-0007 — Idea state lives in frontmatter, not (only) in the database

- **Status:** Accepted
- **Date:** 2026-07-07
- **Deciders:** owner

## Context

Each idea has a lifecycle state (`Draft`, `InDiscussion`, `Stored`, `Reopened` — see
[04-state-machine](../04-state-machine.md)). We must decide where that state is canonically stored.
[ADR-0002](./0002-markdown-source-of-truth-sqlite-index.md) already makes markdown the source of
truth and SQLite a rebuildable index, which constrains this choice.

## Decision

We will store idea state canonically in the **`idea.md` frontmatter** (`state:` field). The index may
hold a copy for querying, but that copy is **derived** and must be reproducible by reindexing. State
transitions write frontmatter first, then upsert the index.

## Consequences

- The vault is **self-describing**: a folder of markdown fully determines every idea's state with no
  database present. Reindex reconstructs all state.
- The frontmatter `state` value is part of the data contract; its serialized form (lower-kebab, e.g.
  `in_discussion`) must match the `IdeaState` enum mapping in [03-data-model](../03-data-model.md).
- A transition is not complete until frontmatter is persisted; index update is best-effort and
  reconcilable.
- Editing `idea.md` by hand (the owner can) can change state; the app must tolerate externally-edited
  frontmatter and re-derive on reindex.

## Alternatives considered

- **State only in SQLite** — simple transactional transitions, but the vault stops being
  self-describing and violates [ADR-0002](./0002-markdown-source-of-truth-sqlite-index.md). Rejected.
- **State inferred from artifacts** (e.g. "has memory/ → Stored") — no explicit field to keep in sync,
  but brittle and ambiguous (a Reopened idea also has memory). Rejected: explicit state is clearer.
