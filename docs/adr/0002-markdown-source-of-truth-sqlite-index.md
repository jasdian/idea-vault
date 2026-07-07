# ADR-0002 — Markdown is source of truth; SQLite is a rebuildable index

- **Status:** Accepted
- **Date:** 2026-07-07
- **Deciders:** owner

## Context

idea-vault must persist ideas, their conversations, and distilled memory. Two forces pull in
opposite directions: the owner wants **human-readable, ownable, git-friendly** artifacts (markdown
they can read and back up without the app), while search/tags/backlinks want **structured, queryable
storage**. We need both without making one undermine the other.

## Decision

We will treat **markdown files on disk as the single source of truth** and use **SQLite (`index.db`)
purely as a derived, rebuildable index** for search, tags, and backlinks. We enforce the
**reindex invariant**: the entire index must be reconstructable from `vault/**` alone, and a
`reindex` operation that rebuilds it from scratch always exists (`index::reindex`).

## Consequences

- Canonical data is never written *only* to SQLite. Every field the index holds must be traceable to
  something in a markdown file (frontmatter or body).
- The index can be deleted at any time and rebuilt; corruption or schema change is recovered by
  reindexing, not by migration gymnastics.
- Writes are dual-path: write markdown first (truth), then upsert the index. If the index write
  fails, truth is intact and reindex will reconcile.
- Search quality depends on the index being fresh; we must reindex on startup-if-drift and on write.
- This invariant is a **testable property** — see [10-testing-strategy](../10-testing-strategy.md).

## Alternatives considered

- **SQLite as source of truth, markdown as export** — better transactional guarantees, but the owner
  loses the "a folder of markdown I own" property and git-diffability. Rejected: violates a core
  product principle ([00-vision](../00-vision.md)).
- **Markdown only, no database** — simplest and fully ownable, but full-text search, tag queries, and
  backlink resolution become slow linear scans. Rejected: search/backlinks are needed and scale
  poorly without an index.
- **Embedded document DB (e.g. sled) instead of SQLite** — capable, but SQLite gives us FTS5 and
  ubiquitous tooling for free. Rejected: SQLite is the better-supported fit.
