# 10 — Testing Strategy

> How the design's invariants are protected by tests once code exists. This is a strategy, not a test
> suite — it names *what* must be tested and *how*, keyed to the invariants the other docs establish.
> No test code is written yet (docs-first); this is the contract the tests will satisfy.

## What must be true (the invariants under test)

| Invariant | Source | How tested |
|-----------|--------|------------|
| Index is fully reconstructable from `vault/**` | [ADR-0002](./adr/0002-markdown-source-of-truth-sqlite-index.md), [D15](./03-data-model.md) | property test (below) |
| State is canonical in frontmatter; re-derivable | [ADR-0007](./adr/0007-state-in-frontmatter-not-db.md), [D9](./04-state-machine.md) | golden-vault + reindex test |
| `conversation.md` is append-only | [D9](./04-state-machine.md) | store/reopen never shrink the file |
| Memory only grows/merges on re-store | [D9](./04-state-machine.md), [D12](./06-concepts/memory.md) | re-store dedupe test |
| Swarm concurrency never exceeds K | [ADR-0006](./adr/0006-bounded-concurrency-swarm.md), [D21](./06-concepts/swarm.md) | semaphore max-in-flight test |
| AI absence degrades, never hangs | [D20](./05-ai-integration.md) | mocked-Ollama absence/timeout test |
| Slugs are unique + stable | [D22](./03-data-model.md) | collision + rename test |
| `[[slug]]` backlinks resolve (incl. forward refs) | [D23](./06-concepts/memory.md) | reindex resolution test |

## The keystone: reindex invariant (property test)

The single most important test, protecting [ADR-0002](./adr/0002-markdown-source-of-truth-sqlite-index.md):

```text
property: for any vault V,
  reindex(V) == reindex(reindex(V))            # idempotent
  and  drop(index); reindex(V)  ==  index(V)   # rebuildable from disk alone
```

Approach: generate randomized vaults (arbitrary ideas, states, tags, memory facts, `[[slug]]`
links — including dangling and forward refs), reindex, snapshot the DB (normalized), reindex again,
and assert equality. Then delete `index.db`, rebuild, and assert the same snapshot. Use the counts
returned by `index::reindex` ([D15](./03-data-model.md)) as a first-line assertion.

## Layered tests

- **Unit (`domain`)** — pure and IO-free, so exhaustively tested: slugify + collisions (D22),
  frontmatter round-trip (D8), `IdeaState` ↔ serialized string mapping, `[[slug]]` parsing.
- **Storage (`vault`)** — against a temp dir: create/read/write `idea.md`, append-only
  `conversation.md`, memory file emit + `MEMORY.md` rebuild. Assert truth-first write order.
- **Index (`index`)** — the reindex property test above, plus query correctness (FTS search, tag
  filter, backlink both-directions) on fixture vaults.
- **AI (`ai`) with a mock Ollama** — a stub HTTP server standing in for `:11434`:
  - streaming path (D11) yields tokens then `done`, assistant turn persisted only on completion;
  - absence / connection-refused / timeout → degradation states (D20), no hang (bounded by timeout);
  - budget assembler respects the size limit and priority order (D21).
- **Concurrency (`concepts::swarm`)** — instrument the semaphore; fan out N ≫ K tasks against the
  mock and assert max concurrent Ollama calls == K and all N complete; a failing agent yields null
  and the judge proceeds (degrade-don't-abort, D14).
- **Web (`web`)** — handler tests over the router: create (D10) produces a `Draft`; store (D12)
  transitions to `Stored` and writes memory; reopen (D13) loads context and sets `Reopened`; SSE
  endpoint emits events; error mapping matches the taxonomy (D24).

## Test doubles & fixtures

- **Mock Ollama** — a local stub implementing `/api/tags` and streaming `/api/chat`, scriptable to
  return tokens, stall (for timeout tests), or refuse connections. This is the seam that keeps the
  suite offline and deterministic despite [ADR-0003](./adr/0003-ollama-local-only-ai.md).
- **Golden vaults** — checked-in fixture `vault/` directories representing each state and edge case
  (dangling backlink, reopened-with-merged-memory, unicode title → slug). Reindex output is snapshot-
  compared.
- **Temp dirs** — storage/index tests run against a throwaway directory, never the real vault.

## What is explicitly not tested by machines

- Prompt *quality* / whether the AI's critique is "good" — subjective, out of scope for automated
  tests; validated by the owner in use.
- Exact token wording from local models — non-deterministic; tests assert *structure and
  persistence boundaries*, not content.

## Related

- [03-data-model](./03-data-model.md) — D15 and the truth/derived contract the property test guards.
- [05-ai-integration](./05-ai-integration.md) — D20/D24 behaviors the AI tests assert.
- [06-concepts/swarm](./06-concepts/swarm.md) — D21 limits the concurrency test enforces.
