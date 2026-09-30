# 10 — Testing Strategy

> How the design's invariants are protected by tests. It names *what* must be tested and *how*,
> keyed to the invariants the other docs establish. The suite exists: in-crate `#[cfg(test)]`
> modules, the integration binaries under `tests/` (sharing `tests/support/`), and the unit tests
> of the `xidea_bench` example. `cargo test` runs all of them.

## What must be true (the invariants under test)

| Invariant | Source | How tested |
|-----------|--------|------------|
| Index is fully reconstructable from `vault/**` | [ADR-0002](./adr/0002-markdown-source-of-truth-sqlite-index.md), [D15](./03-data-model.md) | keystone fixture test (below) |
| State is canonical in frontmatter; re-derivable | [ADR-0007](./adr/0007-state-in-frontmatter-not-db.md), [D9](./04-state-machine.md) | golden-vault + reindex test |
| `conversation.md` is append-only | [D9](./04-state-machine.md) | store/reopen never shrink the file |
| Memory only grows/merges on re-store | [D9](./04-state-machine.md), [D12](./06-concepts/memory.md) | re-store dedupe test |
| Swarm concurrency never exceeds K | [ADR-0006](./adr/0006-bounded-concurrency-swarm.md), [D21](./06-concepts/swarm.md) | semaphore max-in-flight test |
| AI absence degrades, never hangs | [D20](./05-ai-integration.md) | mocked-Ollama absence/timeout test |
| Slugs are unique + stable | [D22](./03-data-model.md) | collision + rename test |
| `[[slug]]` backlinks resolve (incl. forward refs) | [D23](./06-concepts/memory.md) | reindex resolution test |
| An answered plan question is never reopened, by a later re-plan or an audit finding | [ADR-0032](./adr/0032-plan-workbench-answers-and-versions.md), [D33](./06-concepts/skills.md#the-plan-workbench-d33) | `claims::tests::carry_open_findings_skips_answered`, `lineage::tests::suppress_answered_drops_reasked_q_and_rewrites_depends`, `finish::tests::capstone_run_links_to_head_and_suppresses_answered` |
| A plan version never mutates its base | [ADR-0032](./adr/0032-plan-workbench-answers-and-versions.md) | `workbench::tests::base_file_bytes_unchanged`, `tests/plan_workbench.rs` (base bytes unchanged) |
| Re-gating a plan is idempotent (no duplicate markers, no second G10 question) | [ADR-0032](./adr/0032-plan-workbench-answers-and-versions.md) | `plan::tests::regate_is_idempotent`, `workbench::tests::identical_resubmit_returns_existing_version` |
| An identical MCP retry after a served result creates no job, turn or artifact | [ADR-0033](./adr/0033-mcp-idempotent-replay-and-plan-tools.md), [D34](./13-mcp-server-inbound.md) | `tests/mcp_server.rs`: `plain_run_skill_build_prompt_retry_after_served_replays_without_second_plan`, `store_idea_retry_after_served_replays`, `failed_run_is_not_cached_and_retry_runs_again` |
| An `idempotency_key` reused with different arguments is rejected | [ADR-0033](./adr/0033-mcp-idempotent-replay-and-plan-tools.md) | `tests/mcp_server.rs`: `idempotency_key_with_different_args_is_invalid_params` |

## The keystone: reindex invariant (fixture test)

The single most important test, protecting [ADR-0002](./adr/0002-markdown-source-of-truth-sqlite-index.md):

```text
for the fixture vault V:
  reindex(V) == reindex(reindex(V))                  # idempotent
  and  reindex(V) on a fresh index  ==  index(V)     # rebuildable from disk alone
```

It is `index::reindex::tests::keystone_reindex_is_idempotent_and_rebuildable_from_disk_alone`, a
deterministic fixture test, not a randomized property test (there is no proptest dependency).
`build_fixture_vault` writes ideas in several states with tags, memory facts, and `[[slug]]` /
`[[slug#fact]]` links, including dangling and forward refs. The test reindexes into an in-memory
connection, snapshots the DB (normalized, id-free), reindexes again on the same connection and
asserts equal counts from `index::reindex` ([D15](./03-data-model.md)) and an equal snapshot. It
then reindexes into a fresh in-memory connection, standing in for a deleted `index.db`, and asserts
the same snapshot. The snapshot covers every derived table: ideas, tags, memory facts, backlinks,
`fact_links`, `edges` and the FTS rows.

`tests/golden_vault.rs` re-asserts the keystone against the checked-in `tests/fixtures/golden-vault`
and compares its ideas, tags, memory facts, backlinks and FTS rows with `golden-vault.snap`.

## Layered tests

- **Unit (`domain`)** — pure and IO-free, so exhaustively tested: slugify + collisions (D22),
  frontmatter round-trip (D8), `IdeaState` ↔ serialized string mapping, `[[slug]]` parsing.
- **Storage (`vault`)** — against a temp dir: create/read/write `idea.md`, append-only
  `conversation.md`, memory file emit + `MEMORY.md` rebuild. Assert truth-first write order.
- **Index (`index`)** — the keystone reindex test above, plus query correctness (FTS search, tag
  filter, backlink both-directions) on fixture vaults.
- **AI (`ai`) with a mock Ollama** — a stub HTTP server standing in for `:11434`:
  - the model call (D11) returns a complete reply that the caller persists only on success — no
    partial/empty reply ever reaches `conversation.md`;
  - absence / connection-refused / timeout → degradation states (D20), no hang (bounded by timeout);
  - budget assembler respects the size limit and priority order (D21).
- **claude-code backend** — tested against a fake `claude` shell script emitting canned
  `stream-json` (`tests/fixtures/fake-claude.sh`, `tests/claude_backend.rs`); see
  [ADR-0009](./adr/0009-pluggable-llm-backend-claude-code.md).
- **Live backend toggle (`ai::backend::LlmBackend`)** — assert `chat`/`chat_stream`/`probe`/`model`
  dispatch to whichever backend `LlmSettings.backend` currently names, and that a `set_settings`
  call changes the very next dispatch with no reconstruction ([ADR-0011](./adr/0011-live-switchable-llm-backend.md)).
- **Background jobs (`web::jobs`)** — `try_claim` refuses a second concurrent claim for the same
  idea; `peek` reports `Running`/`Failed`/`Idle` correctly and `Failed` is consumed exactly once
  ([ADR-0010](./adr/0010-ai-turns-as-background-jobs.md)).
- **Concurrency (`concepts::swarm`)** — instrument the semaphore; fan out N ≫ K tasks against the
  mock and assert max concurrent calls == K and all N complete; a failing agent yields null
  and the judge proceeds (degrade-don't-abort, D14); with the audit on, the Auditor call is one more
  permit-holding call and the bound still holds (`swarm_flow` keystone runs with the audit on).
- **Web (`web`)** — handler tests over the router: create (D10) produces a `Draft`; store (D12)
  transitions to `Stored` and writes memory; reopen (D13) loads context and sets `Reopened`; the
  chat/skill/swarm routes claim a job and the `/pending` poll reflects job state; error mapping
  matches the taxonomy (D24). The plan workbench (R46-R48, `tests/plan_workbench.rs`) is tested
  against a tempdir vault with no model for the answer path, and a mock Ollama for re-plan.

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

- [03-data-model](./03-data-model.md) — D15 and the truth/derived contract the keystone test guards.
- [05-ai-integration](./05-ai-integration.md) — D20/D24 behaviors the AI tests assert.
- [06-concepts/swarm](./06-concepts/swarm.md) — D21 limits the concurrency test enforces.
