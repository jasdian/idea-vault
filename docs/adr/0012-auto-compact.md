# ADR-0012 — Auto-compact: a fingerprinted `compacted.md` sidecar, folded pre-emptively

- **Status:** Accepted
- **Date:** 2026-07-07
- **Deciders:** owner

> **Amended by [ADR-0014](./0014-dynamic-context-budget.md).** The fixed `AI_BUDGET_BYTES = 16 * 1024`
> constant and the specific "16 KiB" / "~16 KB of ~16 KB" figures below are superseded: the budget
> is now derived live per backend/model (`LlmBackend::context_budget()`), and compaction's targets
> (`memory::compact::CompactTargets`) are the same 0.80/0.40/0.30/1.00 fractions applied to that
> live budget instead of to a constant. Every other decision here — sidecar not truth, prefix
> fingerprint, pre-emptive best-effort fold, bounded high-water advance — is unchanged.

## Context

A discussion that "runs an idea into the ground" grows without bound, but a local model
([ADR-0003](./0003-ollama-local-only-ai.md)) has a small context window. The budgeter
([D21](../06-concepts/swarm.md), `ai::budget`) already trims the oldest whole turns to fit
`AI_BUDGET_BYTES` (16 KiB) — but it did so *silently*: once the transcript exceeded the budget, the
oldest turns dropped off with no record of what they contained, and the usage meter pinned at
"~16 KB of ~16 KB" while quietly discarding the head. A long idea therefore lost its early framing
(decisions, rejected forks) exactly when it mattered most, and the owner had no signal it happened.

We want the discussion to keep fitting the budget *without* losing the head — a rolling summary of
the older turns, the same way an LLM agent harness compacts its own history — while keeping the two
load-bearing invariants intact: `conversation.md` is the append-only source of truth
([ADR-0002](./0002-markdown-source-of-truth-sqlite-index.md)), and the SQLite index is
deterministically rebuildable from markdown.

## Decision

Introduce **auto-compact**: a derived, non-canonical sidecar `vault/<slug>/compacted.md` carrying a
rolling summary of the conversation *head* `turns[0..k]`, made correct by a prefix **fingerprint**
and folded in a background job that runs **before** the reply.

- **Sidecar, not truth.** `compacted.md` (frontmatter: `compacted_through=k`, `covered_bytes`,
  `turn_count_at_compaction`, `model`, `updated` + a four-heading summary body) is a *deletable
  cache*, analogous to how `MEMORY.md` mirrors `memory/*.md`. `conversation.md` is **never** written
  by compaction. A corrupt sidecar reads as absent (rebuilt next fold). It is written via the
  existing crash-safe `write_atomic` (tmp + rename).
- **Prefix fingerprint = self-heal.** `covered_bytes = Σ prefix_bytes(turns, k)` over
  `store::split_turns` (the same splitter `delete_turn` uses; `trim_end().len()+1` per turn). Because
  `conversation.md` is append-only, `turns[0..k]` is immutable under appends, so the summary can
  never go stale from new turns. The only prefix mutation — a `delete_turn` **inside** the summarized
  range — changes `prefix_bytes(turns, k)`, so it no longer equals the stored `covered_bytes`; the
  pure `effective_window` helper detects the mismatch on the next load and falls back to the full
  transcript, and the next fold rebuilds from `k_old = 0`. No cross-module invalidation wiring.
- **Pre-emptive (phase 0 of the chat job), best-effort.** Compaction runs *inside* the already-claimed
  chat job, **before** `run_chat`, so the very turn that tripped the threshold is answered off the
  freshly compacted context. It is strictly isolated: a compaction error is logged and the reply
  proceeds with fallback (uncompacted) context — a compaction failure can **never** turn a good reply
  into `mark_failed`. A manual `POST /idea/{slug}/compact` route (with a "compact now" button) folds
  on demand as its own claim-guarded one-shot job, ignoring the toggle/threshold; refused on `Stored`.
- **Bounded high-water advance (the correctness fix over a naive sidecar).** `choose_high_water`
  advances `k` only over turns whose fold slice fits `COMPACT_SUMMARIZER_INPUT_BYTES` and always
  leaves ≥1 verbatim tail turn — so `k` advances only over turns actually fed to the summarizer, and
  no fold-slice turn is ever silently dropped. Each round folds **only** `turns[k_old..k_new]` merged
  with the prior summary (each turn folded exactly once — no double-count); up to `MAX_FOLD_ROUNDS`
  (4) rounds let a cold/long reopened idea converge in one compaction. Empty model output aborts the
  round with the previous summary intact.
- **Budget fractions of `AI_BUDGET_BYTES`:** trigger threshold `0.80` (of *effective* size, not raw
  `conversation.len()`), verbatim-tail target `0.40`, summary cap `0.30`, summarizer-input bound
  `1.00`. Post-fold effective ≈ 0.30 + 0.40 = 0.70 < 0.80 → hysteresis: a fold moves well under
  threshold and re-arms only as turns accumulate.
- **Excluded from the SQLite index.** Reindex reads only `idea.md` / `conversation.md` /
  `memory/*.md`, so `compacted.md` never reaches the index — the rebuildable-index invariant is
  trivially preserved and no LLM ever runs in the reindex path.
- **Honest meter + full history.** The usage meter reports *effective* bytes (summary + verbatim
  tail) and drops after a fold, annotated "compacted through turn N"; the full transcript is still
  rendered in its entirety, plus a collapsible disclosure revealing the exact summary the model sees.
- **Settings.** `LlmSettings` gains `auto_compact: bool` (default on) and `compact_threshold: f32`
  (default `0.80`, clamped `0.5..=0.95`), live-tunable on the Settings page. Boot defaults via
  `IDEA_VAULT_AUTO_COMPACT` and `IDEA_VAULT_COMPACT_THRESHOLD`.
- **No seal-at-Store.** `extract_and_store` is unchanged and independent: it distils the **full**
  verbatim transcript into durable facts (the high-fidelity backstop against telephone-game drift),
  never substitutes the lossy rolling summary, and never touches `compacted.md` (it must survive to
  be reused on reopen).

## Consequences

- **The head survives a long discussion**, and the meter tells the truth about what the model sees.
- **`delete_turn` / reopen / container restart self-heal** off the fingerprint with zero bespoke
  invalidation code; a persisted sidecar means a reopened 200-turn idea does not eat a cold
  recompaction on every `docker compose up`.
- **Honest concession (invariant safety 4, not 5):** `compacted.md` is *not* byte-deterministically
  rebuildable like `MEMORY.md` (a summary is model output). It is neutralised by being a deletable
  cache, kept out of the index, and truth staying in `conversation.md`.
- **One extra Ollama call on the occasional threshold-tripping turn** (shown honestly via the
  thinking indicator), acquired under the shared `ai_semaphore` ([ADR-0006](./0006-bounded-concurrency-swarm.md))
  like every other call; one compaction per idea via the existing `try_claim` slot. `MAX_FOLD_ROUNDS`
  bounds worst-case calls per compaction.
- **A uniform-byte-length head-delete is a known blind spot:** the byte-count fingerprint cannot
  detect deleting a head turn whose bytes exactly equal another's shift; real transcripts vary in
  size, and Store remains the high-fidelity backstop, so this is accepted.

## Alternatives considered

- **Inline `## compaction` markers appended to `conversation.md`** — fatal: appending places the
  summary *after* the recent turns it must precede (orphaning them), and a mid-file insert violates
  append-only. Rejected.
- **In-memory-only rolling summary (no disk)** — keeps the vault pristine but eats a cold
  recompaction on every restart/reopen, defeating the product's "reopen months later" promise.
  Rejected in favour of the persisted, deletable sidecar.
- **Post-reply (trailing) compaction** — the triggering turn would still be answered from the
  uncompacted (silently-dropped) context, i.e. the original bug. Rejected for pre-emptive phase-0.
- **Seal `k` to `n` at Store** — would add a third Store Ollama call and start reopened ideas with
  zero verbatim tail. Rejected; kept out of scope.

---

> ADRs are immutable once **Accepted**. To change this decision, write a new ADR that supersedes it.
