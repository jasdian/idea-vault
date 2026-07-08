# ADR-0014 — Dynamic, per-backend/model context budget (amends ADR-0012's fixed 16 KiB arithmetic)

- **Status:** Accepted
- **Date:** 2026-07-08
- **Deciders:** owner

## Context

The AI prompt budget was a single hardcoded constant, `AI_BUDGET_BYTES = 16 * 1024`
(`web::routes::mod`, mirrored in `memory::compact` with a drift-guard test), regardless of which
backend or model was active. This was honest for the original small-Ollama-model assumption
([ADR-0003](./0003-ollama-local-only-ai.md)) but wrong in two ways once the backend became
switchable ([ADR-0009](./0009-pluggable-llm-backend-claude-code.md),
[ADR-0011](./0011-live-switchable-llm-backend.md)):

1. **It ignored the model's real window.** An Ollama model with a large native `context_length`
   was still budgeted at 16 KiB; a claude-code model (200k–1M tokens) was budgeted the same as a
   4k-token local model. The usage meter's "~0 KB of ~16 KB" was a fiction in both directions.
2. **Ollama's own server-side default silently truncated anyway.** `chat_with`/`chat_stream_with`
   never sent `options.num_ctx`, so even a well-assembled 16 KiB prompt could be truncated by
   Ollama's own ~4k-token default context window before the model ever saw it — a second, hidden
   budget underneath the one the app already trimmed to.

## Decision

The context budget is now **derived live** from the active backend and model, with a per-backend
manual override, and the derived window is **always sent to Ollama** as `options.num_ctx` so the
assembled budget and the server's actual window agree.

**Resolution order** (`ai::backend::LlmBackend::context_window_tokens`, sync — one lock read plus
one map read, no I/O on the request path):

- **Ollama:** `ollama_ctx_tokens` override (if `> 0`) **else** the model's native window learned
  from `POST /api/show` (cached, keyed by model name) capped at `DEFAULT_OLLAMA_CTX_CAP = 32_768`
  tokens **else**, until the cache has an answer, `FALLBACK_OLLAMA_CTX_TOKENS = 8_192` tokens — the
  exact token count the old fixed 16 KiB constant implied
  (`ContextBudget::for_model_tokens(8192).max_bytes == 16 * 1024`), so a cold cache is
  byte-identical to the pre-dynamic behavior.
- **claude-code:** `claude_ctx_tokens` override (if `> 0`) **else** `claude_window_tokens(model)` —
  the model name matched case-insensitively for a `"1m"` substring (e.g. `sonnet[1m]`) → `1_000_000`
  tokens, else the standard `200_000`. **Deliberately no default cap** on the claude budget: the
  budget here is a ceiling on what the app *assembles into one prompt*, not a VRAM allocation like
  Ollama's `num_ctx` — capping it would just make the meter lie about a window the CLI actually has.
  The `"1m"` match is intentionally loose; a collision is covered by the override.

Bytes are derived from tokens via the existing `ContextBudget::for_model_tokens` (`tokens * 4 / 2`
— roughly 4 bytes/token, half reserved for the model's own output and chat scaffolding), unchanged
from ADR-0012.

**Per-backend, not shared.** `LlmSettings` gains `ollama_ctx_tokens: usize` and
`claude_ctx_tokens: usize` (both `0` = auto) rather than one shared override field — the two
windows differ by one to two orders of magnitude, so the deviation from the original plan's
single-field sketch is intentional. Both are **initial values only**
([ADR-0011](./0011-live-switchable-llm-backend.md) contract): `IDEA_VAULT_OLLAMA_CTX_TOKENS` /
`IDEA_VAULT_CLAUDE_CTX_TOKENS` env vars set the boot value (default `0`, unparsable warns and falls
back to `0`, nonzero clamped `1024..=2_000_000`), and the Settings page (`GET`/`POST /settings`)
exposes "local ctx (tokens)" / "claude ctx (tokens)" fields plus an "effective now: N tokens"
readout, retuned live with no restart.

**`/api/show` + cache + `num_ctx` (Ollama client contract, `ai::ollama`).** `show_context_length()`
issues `POST /api/show` (5s timeout; any transport error, non-2xx, or unparsable body yields `None`
— never a hard error) and `context_length_from_show` scans the response's `model_info` object for a
key equal to or ending in `.context_length` (Ollama's per-architecture key, e.g.
`qwen3.context_length`). `LlmBackend::refresh_ollama_ctx()` caches **both outcomes**, keyed by
model name: a success is cached permanently, and a failure is cached with a
`CTX_PROBE_RETRY_AFTER = 60s` backoff — without the negative cache, an Ollama that answers
`/api/chat` but persistently fails `/api/show` (a proxy, a version lacking the route, a model-name
mismatch) would add the 5s probe timeout to **every** turn, not just the cold first one. The budget
still self-heals: the first dispatch after the backoff re-probes, and a success replaces the
failure entry. It runs from a non-blocking boot
task (`main.rs`, unconditionally, even when claude-code is the active backend, so a live Settings
flip to Ollama finds the window already learned) and from every Ollama chat dispatch (a cold-cache
first turn assembles its prompt at the fallback while that same call's refresh warms the cache for
the next one — one conservative turn, never an over-budget one, accepted). Every `POST /api/chat`
call now **always** sends `options.num_ctx` (via the new `ChatOptions { temperature, num_ctx }`,
replacing the old bare `Option<f32>` temperature parameter) — this is the fix for the silent
Ollama-side truncation described above, independent of whether the app-side budget changed at all.
The dispatched `num_ctx` is additionally **floored at the window the already-assembled prompt
implies** (`ContextBudget::min_window_tokens`, the byte-budget inverse): callers assemble their
prompt against a budget snapshot taken *before* acquiring the shared semaphore
([ADR-0006](./0006-bounded-concurrency-swarm.md)), and a Settings edit while the job is queued must
never shrink the window under a prompt sized against the larger one — that would silently truncate,
the exact failure this ADR eliminates. The floor can never exceed the assemble-time window, so it
adds no VRAM surprise.

**Compaction targets are now ratios of the live budget, snapshotted once per compaction.**
`memory::compact::CompactTargets::for_budget(budget_bytes)` replaces the fixed
`COMPACT_TAIL_TARGET_BYTES` / `COMPACT_SUMMARY_MAX_BYTES` / `COMPACT_SUMMARIZER_INPUT_BYTES`
constants with the same fractions (tail `2/5`, summary cap `3/10`, summarizer input `1/1`) applied
to whichever budget is live when a compaction starts; `run_compaction` reads
`llm.context_budget().max_bytes` **once** and threads the resulting `CompactTargets` through the
gate (`over_threshold`) and every fold round in that compaction, so a Settings flip mid-fold can
never mix targets from two different budgets. The auto-compact trigger is
`compact_threshold × (live budget)`, not a fixed byte number. The old `AI_BUDGET_BYTES` constant
(both copies, `web::routes::mod` and `memory::compact`) and the drift-guard test that kept them
equal are deleted — there is now exactly one source, `LlmBackend::context_budget()`.

## Consequences

- **Amends [ADR-0012](./0012-auto-compact.md)'s fixed 16 KiB arithmetic** — ADR-0012 is not
  rewritten (ADRs are immutable once Accepted); its `AI_BUDGET_BYTES` and the specific "16 KiB" /
  "~16 KB of ~16 KB" numbers it cites are superseded by this ADR's live derivation, while every
  qualitative decision in ADR-0012 (sidecar not truth, prefix fingerprint, pre-emptive best-effort
  fold, bounded high-water advance, the 0.80/0.40/0.30/1.00 *fractions*) is unchanged — only the
  base they're fractions *of* is now dynamic instead of a constant.
- **Meter honesty.** The usage meter (`~X KB of ~Y KB`) now reads `budget_bytes.div_ceil(1024)`
  from `state.llm.context_budget()` on every render (chat, history, idea page) instead of a
  compile-time constant, so the ceiling shown genuinely matches the active backend/model.
- **Budget churn on backend switch is a known, accepted consequence.** Switching Settings backend
  mid-idea changes the live budget the very next compaction reads; because `covered_bytes`
  fingerprinting (ADR-0012) is budget-independent, nothing goes stale — the worst case is one
  extra convergence-fold burst at the new targets, not a correctness bug.
- **New config surface.** `IDEA_VAULT_OLLAMA_CTX_TOKENS`, `IDEA_VAULT_CLAUDE_CTX_TOKENS` (initial
  values, `config.rs`); Settings form fields `ollama_ctx_tokens`/`claude_ctx_tokens` (absent on
  `POST /settings` = keep current value, distinguishing "not submitted" from "explicitly set to
  auto").
- **VRAM guard stays narrow.** `DEFAULT_OLLAMA_CTX_CAP = 32_768` caps only the *auto-derived*
  Ollama window — `num_ctx` allocates KV cache and, with `K` concurrent calls
  ([ADR-0006](./0006-bounded-concurrency-swarm.md)), an uncapped 128k-native model would multiply
  that allocation by `K`. An explicit nonzero `ollama_ctx_tokens` override bypasses the cap
  entirely (still clamped to the outer `1024..=2_000_000` band by config/Settings) — the owner
  asked for it, so the guard steps aside.

## Alternatives considered

- **One shared `context_tokens_override` field for both backends** — the original plan's sketch,
  but the two windows differ by one to two orders of magnitude (thousands vs. hundreds-of-thousands
  of tokens); a single override could never usefully serve both without one backend clamping the
  other's headroom. Rejected in favor of `ollama_ctx_tokens`/`claude_ctx_tokens`.
- **A default cap on the claude-code budget (`CLAUDE_BUDGET_CAP_TOKENS`)**, mirroring the Ollama
  VRAM guard — rejected: there is no local-VRAM analog for a CLI subprocess, and capping it would
  make the meter under-report a window the model genuinely has, defeating the point of deriving the
  budget from the model in the first place. An explicit override remains available if an owner ever
  wants to self-limit.
- **Query `/api/show` synchronously on the request path** — correct but adds network latency to
  every chat/meter render; rejected in favor of a cached, best-effort background refresh that
  self-heals on failure and warms at boot.
- **Never cache failures (retry `/api/show` on every dispatch)** — tempting because a failing
  `/api/show` usually means Ollama itself is down (the chat call fails anyway), but rejected: when
  `/api/show` persistently fails while `/api/chat` keeps working (proxy, older server, model-name
  mismatch between routes), every turn would pay the probe timeout before generation starts,
  forever — D20 says degrade, not silently add latency. Hence the failure cache with the 60s
  retry-after above.

---

> ADRs are immutable once **Accepted**. To change this decision, write a new ADR that supersedes it.
