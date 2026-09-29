# ADR-0031 — Query-driven fact retrieval with snippets: tested, KILLED

- **Status:** Accepted
- **Date:** 2026-09-29
- **Deciders:** owner
- **Relates to:** [ADR-0027](./0027-cross-idea-retrieval-and-the-phase-2-verdict.md) (the shipped
  edge graph and its "Related ideas" block, unchanged by this ADR)

## Context

The ADR-0027 block is computed per idea at reindex time, so every turn of an idea carries the
same related list whatever the owner is discussing. The owner asked whether the Google search
pattern should apply too: treat the latest turn as the query, the other ideas' memory facts as the
documents, rank them with bm25, and show a snippet of the matching passage, pushed **alongside**
the graph block, not instead of it. PageRank was ruled out for now: the vault has no link from
one idea to another, so there is nothing for it to rank.

The owner also agreed that the retriever ships only if a pre-registered experiment shows it beats
the current block.

## Decision

### The experiment

The pre-registration was frozen before any turn-level run (sha256
`ae894f210331da7e5f0f0c47395e1616865a8a86bf063f452a5f724161010519`). It and its results live in
`vault/.eval/xidea-query/`, owner content that never enters the repo. It reuses ADR-0027's frozen
corpus (9 ideas, 68 facts) and verified idea-pair labels (5 related, 27 unrelated, 4 uncertain).

- **Queries:** the 33 owner (`## user`) turns (primary); all 110 turns (reported only).
- **graph:** `memory::related::related_entries(A)` at main 3461276, the shipped block.
- **query:** `index::queries::turn_fact_hits(A, turn)`: the 12 highest-IDF usable terms of the
  turn, OR-ed against the other ideas' memory-fact rows of the eligible-only bm25 corpus; a fact
  needs 2 distinct query terms; best fact per idea, at most 3; FTS5 `snippet()` for the passage.
- **Item:** one pushed idea on one turn. TP = the pair is labelled related, FP = unrelated.
- **Ship criterion (all):** C1 precision gain ≥ 0.10 over graph; C2 ≤ 1.0 FP per turn;
  C3 ≥ 3 TP; C4 C1–C3 hold without the 6 contaminated facts; C5 C1 and C2 hold in ≥ 80% of 1000
  label-flip draws (p = 0.2).

### Result

| queries | retriever | turns | TP | FP | uncertain | novel TP | precision | FP per turn |
|---|---|---|---|---|---|---|---|---|
| owner | graph | 33 | 26 | 39 | 25 | — | 0.400 | 1.182 |
| owner | query | 33 | 4 | 9 | 2 | 0 | 0.308 | 0.273 |
| all | graph | 110 | 90 | 126 | 83 | — | 0.417 | 1.145 |
| all | query | 110 | 6 | 12 | 2 | 0 | 0.333 | 0.109 |

- C1 fails (0.308 vs 0.400, a loss of 0.09, not a gain of 0.10). C2 and C3 hold. C4 fails
  (identical numbers: no turn hit a contaminated fact). C5: 223/1000 = 22.3% of draws pass.
- **Novel TP is 0:** every correct query hit named an idea the graph block already shows.
- Most owner turns (23 of 33) return nothing; the FPs come from ordinary words that pass the
  two-term guard ("does check", "user when", "rather").

**Verdict: KILLED.** The query section is not pushed into any prompt and no route, concept or
template calls the retriever.

### What stays

- `index::queries::turn_fact_hits` stays as an offline instrument next to `vault_search`, with
  its unit tests. `tests/vault_search_isolation.rs` asserts that neither appears in a model tool
  list, the MCP `tools/list` or `src/ai/backend.rs`, and that no module outside `src/index/`
  references either.
- The harness `examples/xidea_query_bench.rs` stays as a test-built example outside the binary.

**Revisit trigger**, as ADR-0027's: the thresholds are not re-tuned on this corpus; revisit at
~30–50 ideas or after a named real-use miss of the shipped panel.

## Consequences

- Nothing changes for the owner: the ADR-0027 block and panel are exactly as before.
- The hits that were right read well. Examples: the Azure Service Bus precedent for a banks
  question, "Money-state should be derived from transfers" for a ledger-state question, and the
  TigerBeetle PII limit for a PII question. All of them came from ideas the graph already lists.
  So the turn picked better **facts**, not better **ideas**. A different design, where the graph
  still chooses the ideas and the turn only reorders which of their facts the `Facts:` line
  shows, cannot add an unrelated idea. It is untested, needs its own pre-registration, and is
  left to the owner.
- A stopword list or a 3-term guard would likely cut the FPs, but choosing either after seeing
  these results would be tuning on the test set. Neither is applied.

## Alternatives considered

- **Ship it anyway, alongside the graph.** Rejected: it lost to the graph on precision, found no
  related idea the graph missed, and the owner made shipping conditional on the experiment.
- **PageRank over the link graph.** Not built: there are 0 cross-idea links to rank.
- **Replace the graph with the query retriever.** Not tested as a ship option; the owner asked for
  "alongside, not instead", and the query retriever is the weaker of the two here.

---

> ADRs are immutable once **Accepted**. To change a decision, write a new ADR that supersedes this
> one.
