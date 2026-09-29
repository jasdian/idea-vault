# 06 — Concept: Memory

> The feature that makes an idea *resumable*: distil durable facts when an idea is **Stored**, reload
> them as context when it is **Reopened**. Mirrors an LLM agent's file-based memory. Home of **D12**
> (extraction), **D13** (reload), **D23** (backlink resolution). Module: `memory`.

## Model

- A **memory fact** is one durable conclusion about an idea: a single file `memory/<fact-slug>.md`
  with frontmatter + a short body. One fact per file — small, atomic, individually linkable.
- **`MEMORY.md`** is a one-line-per-fact index, loaded as the cheap always-on context when an idea
  reopens (the fact bodies are pulled in selectively under budget).
- Facts may reference other ideas with `[[slug]]` links, resolved into the `backlinks` table on
  reindex ([D23](#d23--slug-backlink-resolution)).

This deliberately matches the agent-harness convention: file-per-fact, a loaded index, `[[slug]]`
cross-links — see [00-vision](../00-vision.md).

## D12 — Store → memory extraction

Fires on the `InDiscussion→Stored` (or `Reopened→Stored`) transition ([D9](../04-state-machine.md)).
Consolidates the idea and distils memory. Like every other model-calling route, the two AI calls
run as a detached, claim-guarded **background job** ([ADR-0010](../adr/0010-ai-turns-as-background-jobs.md)):
the request returns immediately with the transcript + a "thinking" indicator, and the owner-visible
Stored view only appears once the job lands and the browser's next poll picks up the state flip.

```mermaid
sequenceDiagram
    autonumber
    participant U as Owner ("store it")
    participant H as web::routes::memory (store)
    participant J as web::jobs
    participant Task as detached tokio task
    participant Ex as memory::extract
    participant AI as ai (Ollama)
    participant V as vault::store
    participant Idx as index
    participant B as Browser (HTMX, polling)

    U->>H: POST /idea/:slug/store
    H->>H: guard state (D9): InDiscussion needs ≥1 turn; Draft/Stored rejected
    H->>J: try_claim(slug) — one job per idea
    H-->>U: 200 transcript + "thinking…" indicator (self-repolling)
    H->>Task: tokio::spawn (detached — outlives the request)
    Task->>Ex: extract_and_store(idea, conversation)
    Ex->>V: read existing memory/*.md (empty on a first store)
    Ex->>AI: prompt: consolidate best statement
    AI-->>Ex: consolidated statement
    Ex->>AI: prompt: extract facts from the CONSOLIDATED statement + transcript, shown existing facts as [[slug]] — title
    AI-->>Ex: FACT / OP (ADD | UPDATE slug | NOOP) / QUOTE / body blocks
    Ex->>Ex: evidence gate — each QUOTE must occur (normalized) in conversation.md or the pre-store body
    Ex->>Ex: NOOP skipped; UPDATE → append to that fact; ADD with an existing slug → skipped (backstop)
    alt both AI calls succeed
        Ex->>V: write idea.md body (consolidated), state=stored
        Ex->>V: write new memory/<fact-slug>.md + appended UPDATEs
        Ex->>V: rebuild MEMORY.md index
        opt any fact failed the gate
            Ex->>V: write artifacts/<stamp>-quarantined-facts.md (kind: quarantine)
        end
        Ex->>Idx: upsert ideas, memory_facts, backlinks, search_fts
        Task->>J: mark_done(slug) — or mark_notice(slug, "Stored — but N facts quarantined / context truncated")
    else failure (either AI call)
        Task->>J: mark_failed(slug, message)
    end
    loop every ~1.5s until Idle
        B->>H: GET /idea/:slug/pending
        H->>J: peek(slug)
        H->>V: read idea state
        alt state == Stored
            H-->>B: Stored view (partial) + one-shot notice (if any) + OOB badge, HX-Retarget #discussion
        else still running / failed
            H-->>B: re-emit "thinking…" | error block
        end
    end
```

Rules:

- **Consolidate then distil:** the idea body is rewritten to the current best statement *before*
  facts are extracted. The extraction call reads that consolidated statement, so facts reflect
  conclusions, not the idea as first pitched.
- **Bounded set:** extraction targets a small number of high-value facts (at most 7), not a
  transcript dump. That respects context and readability; the conversation already holds the full
  detail.
- **Evidence gate** ([ADR-0023](../adr/0023-verification-layer.md)): every fact must carry a
  `QUOTE:` copied word for word from the discussion — at least 3 words. A `…` may elide a few
  words, but the segments must appear in order and close together, so two unrelated true fragments
  can't vouch for a spliced claim.
  - Code checks the quote against the raw `conversation.md` and the pre-store idea body, after
    folding case, typographic quotes and dashes, markdown marks and whitespace. The consolidated
    body doesn't count: the model just wrote it.
  - A fact that fails the gate is **quarantined**, not remembered. It goes into
    `artifacts/<stamp>-quarantined-facts.md` (artifact kind `quarantine`, labelled "quarantined
    facts · unverified"), which the owner can read and copy from by hand.
  - The stored view carries a one-shot notice saying how many facts were quarantined, and whether
    the discussion was too long for the model to read in full.
- **Merge on re-store:** the extractor is shown the facts already in memory and answers per fact:
  - `ADD` — a new fact;
  - `UPDATE <slug>` — **appended** to that fact under an `_Updated <date>:_` line; the existing
    text, which the owner may have edited, is never replaced;
  - `NOOP` — already captured.

  An `ADD` whose title slugifies to an existing fact is still skipped as a backstop. Memory only
  grows or consolidates, never silently drops ([D9](../04-state-machine.md) invariant).
- **Cross-links are made canonical by code:** the model can't know fact slugs, so it references a
  related fact by title in `[[…]]`. `memory::extract::cross_link` rewrites every reference that
  matches a known fact (this batch or already on disk) to `[[that-slug]]`, auto-links an
  unbracketed mention of another fact's exact title (only titles of at least 12 characters), and
  records the targets in the fact's frontmatter `links`. Unknown references stay verbatim and
  dangle legally ([D23](#d23--slug-backlink-resolution)).
- **Tags are suggested, only ever added:** the extraction answer ends with one
  `TAGS: a, b, c` line. `memory::extract::parse_tags` slugifies it, dedupes it and caps it at
  `MAX_NEW_TAGS` (5) per store. New tags are merged into the idea's frontmatter `tags:`, never
  replacing an owner-set tag, until the idea holds `domain::frontmatter::MAX_IDEA_TAGS` (10), the same
  ceiling the owner's tag editor enforces. Reindex mines them from `idea.md` like any other tag.
- **Truth first:** markdown written before index upsert ([ADR-0002](../adr/0002-markdown-source-of-truth-sqlite-index.md)).
- **Nothing partial on failure/cancel:** truth is only touched after both AI calls succeed — an
  aborted or failed job leaves the idea in its prior state, still `InDiscussion`/`Reopened`
  ([ADR-0010](../adr/0010-ai-turns-as-background-jobs.md)).
- **One job per idea:** `try_claim` refuses a second concurrent job for the same idea, same as
  chat/skill/swarm ([D11](../05-ai-integration.md)).

## D13 — Reopen → load memory as context

Fires on `Stored→Reopened`. Reassembles context so the AI "remembers", within budget.

```mermaid
sequenceDiagram
    autonumber
    participant U as Owner (reopen)
    participant H as web::routes::memory (reopen)
    participant Ld as memory::load
    participant V as vault::store
    participant Cp as memory::compact
    participant Bud as ai::budget

    U->>H: POST /idea/:slug/reopen
    H->>H: guard: only a Stored idea reopens (else 400)
    H->>Ld: load_context(slug, budget)
    Ld->>V: read MEMORY.md (index of facts)
    Ld->>V: read memory/*.md, newest first (by created)
    Ld->>V: read_compacted — compacted.md, if any
    Ld->>Cp: effective_window(turns, compacted)
    Cp-->>Ld: fingerprint valid → summary + verbatim tail · else → full transcript
    Ld->>Bud: assemble: idea body → facts → summary (whole or dropped) → recent turns, under budget (D21)
    Bud-->>Ld: budgeted context block (+ inclusion counts, logged)
    H->>V: set state=reopened (write_idea), then reindex
    H-->>U: discussion view + OOB state badge
    Note over H,Bud: every later turn (D11) re-runs load_context — the same assembly, fresh from disk
```

Rules:

- **Index first, bodies selectively:** `MEMORY.md` is always loaded (cheap); full fact bodies are
  pulled newest first (by frontmatter `created`; there is no relevance ranking) only up to the
  budget — on small local models this matters ([ADR-0006](../adr/0006-bounded-concurrency-swarm.md)).
- **Reopen validates, the turn assembles:** `memory::load::load_context` runs at reopen only to
  check the context and log what fits. The context the model actually sees is rebuilt by the same
  function on every chat turn (`chat::run_chat`), so a memory edit or a new fact counts from the
  next turn on.
- **A compacted head replaces old turns:** if `compacted.md` exists and its `covered_bytes`
  fingerprint still matches the transcript prefix, `memory::compact::effective_window` feeds its
  rolling summary plus the turns after it in place of the full history
  ([ADR-0012](../adr/0012-auto-compact.md); the budget it fills is ADR-0014's). The summary is atomic:
  `ai::budget::assemble_context` includes it whole after the idea body and facts, or drops it whole.
  If the fingerprint doesn't match (a turn inside the folded range was deleted), the full transcript
  is used instead: the stale summary is ignored, never trusted.
- **Reopen is truth-idempotent:** it loads context and flips state; it does **not** rewrite body or
  memory (those change only on Store — [D9](../04-state-machine.md)).

## D23 — `[[slug]]` backlink resolution

Facts and bodies can reference other ideas. Links are resolved during reindex ([D15](../03-data-model.md)),
not at write time, so forward references (to not-yet-created ideas) are allowed.

```mermaid
flowchart TD
    A["reindex walks vault/**"] --> B["scan idea body + memory/*.md for [[slug]] and [[idea#fact]]"]
    B --> C["insert backlinks(source_idea_id, target_slug)<br/>+ fact_links candidates"]
    C --> D{"target slug exists in ideas?"}
    D -- yes --> E["set target_idea_id"]
    D -- no --> F["leave target_idea_id NULL (unresolved)"]
    E --> G["backlinks queryable both directions"]
    F --> G
    G --> H["later reindex re-resolves once target created"]
    C --> I{"fact_links: dst fact exists?"}
    I -- yes --> J["set dst_fact_id"]
    I -- "no, explicit [[idea#fact]]" --> K["keep row, dst_fact_id NULL (dangling)"]
    I -- "no, bare candidate" --> L["drop row (it named an idea, not a fact)"]
```

Link rules, as `index::reindex` applies them (parsing is `domain::links`):

- **Syntax.** `[[slug]]` targets an idea. `[[idea#fact]]` targets one memory fact inside an idea:
  exactly one `#`, and both halves must be canonical slugs, otherwise the token is prose. The two
  forms are disjoint in `domain::links`. Conversation and artifact bodies are never mined.
- **Bare `[[token]]` inside a fact** (and a fact's frontmatter `links:` entry) is a candidate for
  both readings: an idea backlink to `token`, and a `fact_links` candidate for a sibling fact
  `token` in the same idea. A candidate that resolves to no fact is dropped from `fact_links`;
  the `backlinks` row stays.
- **`[[idea#fact]]`** in the idea body or a fact body always keeps its `fact_links` row
  (`explicit = 1`), resolved or dangling, and also adds an idea backlink to `idea`.
- **Self-refs are excluded.** A fact never links to itself, and a `[[idea#fact]]` into the
  fact's own idea adds no backlink. Edges never pair an idea with itself.
- Resolved backlinks and cross-idea `fact_links` become `link` edges of weight 1.0
  ([D6](../03-data-model.md)), the strongest cross-idea signal for the related block.

## Mapping to code

| Piece | Location |
|-------|----------|
| Extraction on Store (D12) | `memory::extract` |
| Reload on Reopen (D13) | `memory::load` |
| Rolling summary of the head (D13, ADR-0012) | `memory::compact` + `vault::store::read_compacted` |
| Backlink parse/resolve (D23) | `memory::backlinks` + `index::reindex` (pure parsing in `domain::links`) |
| Related-ideas block (chat, skills, swarm, workflows) | `memory::related` |
| Fact type | `domain::memory::MemoryFact` |
| On-disk shape | [03-data-model](../03-data-model.md) D7/D8 |

## Related

- [04-state-machine](../04-state-machine.md) — where D12/D13 fire.
- [swarm](./swarm.md) — D21 budgeting used by load/extract.
