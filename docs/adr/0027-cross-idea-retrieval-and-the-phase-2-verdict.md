# ADR-0027 — Cross-idea retrieval: a derived edge graph, pushed; embeddings killed

- **Status:** Accepted
- **Date:** 2026-09-29
- **Deciders:** owner
- **Amends:** [ADR-0014](./0014-dynamic-context-budget.md) (adds the leftover-only related allowance); [ADR-0002](./0002-markdown-source-of-truth-sqlite-index.md) (derived-schema drift is detected by the `SCHEMA_VERSION` stamp)

## Context

Before this change the foil saw one idea at a time. `memory::load::load_context` and
`concepts::skills::hydrate_context` each take a single slug, so a chat, skill, swarm or workflow
prompt about idea A carried nothing from idea B, even when B had already settled the same
problem. The index had some cross-idea signal, but none of it reached the model:

- `backlinks` was indexed and used only as a ranking prior in `search`.
- Fact `[[fact-slug]]` links were written into `backlinks` as idea-level targets. They never
  matched an idea and stayed unresolved, and `[[idea#fact]]` was not parsed at all.
- Tags drifted (`system-design` / `systems-design`) with nothing reporting it.

The owner asked for the vault's own links, tags and shared vocabulary to be pushed into each
idea's context as a small "Related ideas" block, with no new store, no vector DB and no model call
at reindex or boot ([INTENT.md](../INTENT.md), "Intent — cross-idea retrieval"). They also asked
for an honest experiment to decide whether embeddings (phase 2) are worth building at all. That
experiment was pre-registered before any label or retriever output existed, with a fixed kill
criterion. Its artefacts live in `vault/.eval/xidea/`, which is owner content and never enters
the repo.

## Decision

### Phase 0/1: shipped

Everything below is derived from `vault/**` by `reindex` and lives in `index.db`
([ADR-0002](./0002-markdown-source-of-truth-sqlite-index.md)). Deleting `index.db` and
reindexing reproduces it exactly, and the in-crate keystone test (`index::reindex`) snapshots the new tables.

- **Fact-level links.** `domain::links::extract_fact_refs` parses `[[idea#fact]]`. The references
  resolve into a new `fact_links` table (source idea and fact, destination idea and fact slug,
  resolved `dst_fact_id`, `explicit`). An explicit `[[idea#fact]]` also counts as an idea-level
  backlink to `idea`. Existing `backlinks` semantics are otherwise unchanged.
- **One derived `edges` table** of idea pairs, stored canonically (`CHECK (src_idea_id <
  dst_idea_id)`, own pair never stored), with three `type`s:
  - `link`: weight 1.0, from resolved backlinks and resolved cross-idea `fact_links`.
  - `tag`: exact tag names only. A tag carried by `df` of `N` ideas weighs `0.3·ln(N/df)/ln(N)`,
    so a tag every idea carries weighs nothing. A pair weighs the sum over its shared tags, capped
    at 1.0.
  - `lexical`: word overlap from the fair lexical baseline (below). Each idea proposes its top 3
    hits (`LEXICAL_EDGE_TOP_K`). A hit is kept only if at least 2 of the proposer's query terms
    (`LEXICAL_EDGE_MIN_SHARED`) occur in the other idea after two guards: a term every idea
    contains (IDF 0) does not count, and for a pair already joined by a `link` edge, the words of
    both slugs do not count. A kept hit weighs `0.19 · score / top score`
    (`LEXICAL_EDGE_WEIGHT`), so it never exceeds 0.19, sits below a typical tag weight, and a two-hop
    lexical-only path stays under the display floor.
- **`index::queries::related_ideas`**, a recursive CTE over the undirected edges up to two hops.
  A pair scores the sum of its edge types. A direct neighbour scores its pair weight; a two-hop
  idea scores 0.5 × its best path, where a path weighs as much as its weakest link. Each idea is
  scored at its minimal hop count only.
- **A push-injected "Related ideas elsewhere in the vault" block** (`memory::related`) for chat,
  skill, swarm-angle and workflow-stage prompts. It shows at most `MAX_RELATED` = 5 ideas and
  drops any scoring below `MIN_RELATED_SCORE` = 0.1. It has its own budget: it is assembled
  *after* the idea's own context and gets only the leftover bytes, capped at
  `min(max_bytes / 10, RELATED_CAP_BYTES = 2048)` (`ai::budget::related_allowance`,
  [ADR-0014](./0014-dynamic-context-budget.md)). The idea's own assembled text is byte-identical
  with and without the block, and `load_context` / `assemble_context` are unchanged. Audit,
  synthesis and knowledge extraction never receive the block: those call paths do not take it
  as a parameter.
- **The related panel** on the idea page (`templates/_related.html`) renders from the same
  `memory::related::related_entries` builder as the block, so the two use the same floor, cap
  and redaction.
- **Tag drift is surfaced, never merged.** Near-duplicate tag pairs (`domain::tag::near_duplicate`)
  are listed on the related panel. They never become an edge and are never fuzzy-matched.
- **The own slug is excluded everywhere**: the edges query, the lexical baseline, `vault_search`
  and the experiment's embeddings. Each exclusion has a test (the edges query only indirectly,
  through the cycle-back and two-hop fixtures).
- **No new store, no model call.** There is no vector DB, no graph DB and no store outside
  `index.db`. Reindex and boot make no model call: the index is derived purely from markdown
  ([ADR-0002](./0002-markdown-source-of-truth-sqlite-index.md)), and [ADR-0012](./0012-auto-compact.md)
  keeps model output such as `compacted.md` out of the reindex path.
- **Schema stamp.** `schema::SCHEMA_VERSION` is stamped into `PRAGMA user_version` by a completed
  reindex and is now **6**. An index with a different stamp counts as drift at boot, and reindex
  drops and recreates every derived table before rebuilding. The derived tables are never
  migrated.

### Phase 2 (embeddings): KILLED

The phase-2 experiment ran `embeddinggemma` (digest `85462619ee72`) through Ollama
against a frozen copy of the vault: 9 ideas, 68 facts, 36 unordered pairs. Three retrievers
each proposed a top 3 per idea:

- `lexical`: the fair baseline;
- `embed`: centroid-subtracted cosine;
- `fused`: RRF with k = 60.

A true positive (TP) is a proposed pair labelled `related`. The verified labels are 5 related,
27 unrelated and 4 uncertain.

| corpus | TP lexical / embed / fused | margin |
|---|---|---|
| full | 4 / 5 / 4 | 1 |
| minus the 6-file contamination set (kill condition 2) | 4 / 5 / 5 | 1 |
| minus the 2-file own-text set (sensitivity only) | 4 / 5 / 5 | 1 |

- **Condition 1 fails.** It needs `max(embed, fused) − lexical ≥ 2` on the full corpus; the
  margin is 1.
- **Condition 2 holds.** It needs a margin of at least 1 once the 6-file contamination set is
  removed (2 files carry "cheapest disproof" in their own text, 4 only through `[[…]]` link slugs
  or `links:`); the margin is 1.
- **Centroid safeguard.** The mean pairwise cosine among the 6 contaminated facts is 0.4447 before
  centroid subtraction and 0.1341 after.
- **Single flips.** 13 of the 14 single label flips still give KILLED. The one PASS flips
  impactful-for-the-ecosystem × use-vector-db-… from uncertain to related. That pair was refuted
  by 2 of 3 adversarial refuters as a shared reasoning method only.
- **Bootstrap.** Each scored label was flipped with p = 0.2 over 1000 seeded draws. 266/1000 =
  26.6% of draws pass both conditions, below the pre-registered 80% bar. Phase 2 needs the point
  verdict *and* ≥80% of draws to pass.

**Verdict: phase 2 is KILLED.** No embedding code ships. The harness (`examples/xidea_bench.rs`)
stays in the repo as a test-built example outside the shipped binary. Its cache and results stay
in `vault/.eval/xidea/`.

**Revisit trigger**, verbatim from the pre-registration: "Do not rerun this 9-idea experiment;
revisit only at ~30–50 ideas or after a named real-use miss of the shipped phase-1 panel."

**Caveats.**

- The evidence is small: 9 ideas and 5 positives.
- There is no human ground truth (owner ruling). The labels come from two Claude panels: L1 read
  the full markdown, and L2 was blind, seeing only idea bodies without tags plus fact bodies.
  Fleiss κ within the panels was 0.829 (L1) and 0.792 (L2). L1 and L2 majorities agreed 94.4%,
  Cohen κ 0.840. The related pairs were then adversarially verified.
- The pre-registration itself calls the result a plausibility check, not proof.

### Instrument notes

- `index::queries::vault_search` (fact-level) and `index::queries::lexical_baseline` (idea-level)
  are offline instruments. Since commit d2a3098 both score **bm25** against a connection-local TEMP
  FTS5 copy of the eligible rows only (`temp.lexical_fts`: kinds `title`, `tags`, `idea_body`,
  `memory`). Conversation and artifact rows therefore neither match nor shape IDF or average row
  length. That copy still holds the excluded idea's own rows. Exclusion filters the *results*,
  not the statistics, so the queried idea still contributes to IDF and average length.
- Neither is ever a model tool. Context reaches the model by push, never by pull.
  `tests/vault_search_isolation.rs` asserts that `vault_search` is absent from the web and source
  tool definitions, from the MCP `tools/list` and from `src/ai/backend.rs`. A source walk also
  asserts that nothing outside `src/index/` references it. `lexical_baseline` has no such test;
  its reindex use goes through the shared `LexicalCorpus`, not through a tool.

## Consequences

- Every idea's foil now sees its neighbours' titles, reasons and latest fact titles. The block
  can only use budget the idea's own context left unused, so a full own context gets no block.
- **Known consequence: lexical edges are dense and weak at this vault size.** The per-signal
  measurement on the frozen corpus, rebuilt at schema version 6 (the shipped code), found:
  - Explicit links gave 0 pairs. None of the corpus's `[[…]]` links resolves to another idea.
  - Shared tags gave 2 pairs, both labelled unrelated.
  - Lexical edges cover 19 of 36 pairs, with recall 3/5. Precision is 3/19, or 3/16 when
    uncertain pairs are left out.
  - 18 of the 19 lexical edges pass the 0.1 display floor. That is 4.0 shown neighbours per idea,
    of which about 0.7 are correct.
  - The first measurement, at commit 90efbe5 (schema 5, before bm25 was restricted to the
    eligible rows), gave 20 pairs with recall 5/5; the eligible-only corpus moved two related pairs
    out of the edges' top 3. (The experiment's lexical *retriever* is scored separately above,
    4 of 5 related pairs in its top-3 proposals.)

  The block and the panel are therefore noisy today. The owner ruled that lexical edges ship
  unconditionally. These tuning options are left open for the owner and **not applied**:
  - lower `LEXICAL_EDGE_TOP_K` to 2;
  - raise `LEXICAL_EDGE_MIN_SHARED` to 3;
  - set a higher display floor for lexical-only pairs than `MIN_RELATED_SCORE`.
- Shared-source and time-proximity edges were measured and not built. No two ideas share a
  source, and there was no evidence that creation time predicts relatedness.
- A persisted chat or synthesis turn that names a related idea can be remembered at store time
  under [ADR-0023](./0023-verification-layer.md)'s verbatim rule, as any chat content can.
- Any change to the edge derivation must bump `SCHEMA_VERSION` so existing indexes rebuild on the
  next boot.
- The two instruments rebuild the eligible-only copy on every call, O(corpus) work. That is
  acceptable offline and one reason they stay off the request path.
- Retrieval is sampling-agnostic. Per-role profiles ([ADR-0026](./0026-per-role-call-profiles.md))
  change how a prompt is sampled, never whether it carries the block.

## Alternatives considered

- **A vector DB** (external or embedded). Rejected: it is a second store that reindex cannot
  derive without a model call, and the phase-2 experiment gave no evidence that embeddings beat
  the lexical baseline by the pre-registered margin.
- **`sqlite-vec` inside `index.db`.** Rejected: it would have to be loaded as a SQLite extension,
  which needs `unsafe` in rusqlite, and the invariant ratchet holds `UNSAFE_FLOOR=0`. It would
  also still need a model call to populate. Phase 2 is killed in any case.
- **A graph DB.** Rejected: two-hop related ideas are one recursive CTE over `edges` in the
  SQLite index that already exists. A new store would break the single-binary, rebuildable-index
  shape ([ADR-0002](./0002-markdown-source-of-truth-sqlite-index.md)).
- **Pull-style model tools (`vault_search` as a tool).** Rejected: the owner wants neighbours
  pushed, deterministically and on a fixed budget, not left to a local model to decide to search.
  A pull tool would also let the model spend context on other ideas at will. `vault_search`
  stays an offline instrument, enforced by test.
- **Fuzzy tag merging.** Rejected: a one-character difference is often meaningful (`series-a` /
  `series-b`, `hiring` / `firing`). Near-duplicates are surfaced for the owner to fix in the
  markdown, never merged silently.

---

> ADRs are immutable once **Accepted**. To change a decision, write a new ADR that supersedes this
> one and update the Status line above.
