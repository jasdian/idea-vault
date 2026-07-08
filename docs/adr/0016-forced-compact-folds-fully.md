# ADR-0016 — Forced compaction folds to a zero tail target, and a genuine no-op is a visible notice

- **Status:** Accepted
- **Date:** 2026-07-08
- **Deciders:** owner

> **Amends [ADR-0012](./0012-auto-compact.md).** Its sentence that the manual route "folds on
> demand as its own claim-guarded one-shot job, ignoring the toggle/threshold" undersold what
> "ignoring" meant: `force = true` only skipped the settings threshold *gate* — the fold still
> targeted the same `tail_target_bytes` (0.40 of the live budget) as the automatic phase-0 path.
> This ADR changes what a forced fold targets and adds an honest no-op signal; every other decision
> in ADR-0012 (sidecar not truth, prefix fingerprint, pre-emptive best-effort auto fold, bounded
> high-water advance, the 0.80/0.30/1.00 fractions on the *automatic* path) is unchanged.

## Context

ADR-0012's manual `POST /idea/{slug}/compact` route (the "compact now" button) was meant to force a
fold on demand, refused only on `Stored`. It set `force = true` to bypass the `auto_compact`
toggle and the `compact_threshold` gate, but `choose_high_water` still advanced only until the
verbatim tail reached `tail_target_bytes` — the same 0.40-of-budget target the automatic phase-0
path uses to decide when it has folded *enough*, not an instruction to fold *everything it can*.

Under [ADR-0014](./0014-dynamic-context-budget.md)'s dynamic budgets this made the button silently
useless in the common case. A claude-code budget is ≈200k tokens ≈400 KB, so 0.40 of it is ≈160 KB
— larger than most conversations ever get before the owner reaches for the button. `choose_high_water`
never had anything to advance past, so the job completed in milliseconds: no model call, no
`compacted.md` write, no visible change, and no error either. The owner saw "thinking…" flash and
disappear, with no signal whether the fold happened, failed, or was a no-op — indistinguishable from
a bug.

## Decision

We will make "compact now" actually compact, and make an honest no-op visible instead of silent.

- **`CompactTargets::forced(budget_bytes)`** — a second constructor alongside `for_budget`, identical
  in every field except `tail_target_bytes: 0`. The manual route now builds its targets with
  `forced`, not `for_budget`. `choose_high_water`'s existing guards are unchanged and still apply:
  it always leaves at least one verbatim tail turn, still bounds each fold slice by
  `COMPACT_SUMMARIZER_INPUT_BYTES`, and `MAX_FOLD_ROUNDS` still caps worst-case model calls per
  compaction. A forced fold therefore advances `k` as far as those guards allow — everything except
  the final turn — rather than stopping at a tail sized for steady-state chat. The automatic
  phase-0 path (`for_budget`, `tail_target_bytes` = 0.40) is byte-identical to before this ADR.
- **`run_compaction` returns `Result<CompactOutcome, String>`**, where
  `CompactOutcome::{Folded, NothingToFold}` replaces the old `Result<(), String>`. `NothingToFold`
  covers the two genuine no-op cases even under a zero tail target: a single-turn conversation, and
  a conversation already folded up to its last turn. The automatic caller (`maybe_run_compaction`,
  the phase-0 pre-reply path) discards the distinction and keeps its original `Result<(), String>`
  signature — an auto no-op is expected steady-state behavior, not something the owner needs to see
  on every reply.
- **`JobStatus::Notice(String)` / `Pending::Notice(String)`** — a new one-shot job outcome,
  read-once-then-cleared by the next poll exactly like `Failed`. The manual compact route maps
  `CompactOutcome::NothingToFold` to `jobs::mark_notice`, rendered by a new `notice_block` partial
  ("nothing to fold — the conversation is a single turn or already compacted", `.foil-notice` CSS)
  with neutral, non-error styling — distinct from the red `Failed` error block, because nothing went
  wrong.
- **Tracing.** `run_compaction` logs `info!` per folded round (slug, force, round,
  compacted_through) and `debug!` when it returns `NothingToFold`, so the two outcomes are visible
  in logs as well as the UI.

## Consequences

- **"Compact now" now does what its name says** on any conversation with more than one turn not
  already folded, independent of how large the live budget is — the button's behavior no longer
  degrades as ADR-0014's dynamic budgets grow.
- **A genuine no-op is now distinguishable from "broken"** — the owner gets a visible, worded notice
  instead of a flash of "thinking…" and silence, without it being mis-rendered as a `Failed` error.
- **New enum variant to thread through.** `JobStatus`/`Pending` gain a third read-once branch;
  every match over them (the poll handler, tests) must account for `Notice` alongside `Running`/
  `Idle`/`Failed`. `web/jobs.rs` already covers this with a drift test
  (`mark_notice_is_consumed_exactly_once`).
- **`run_compaction`'s signature change is call-site-visible.** The manual route now matches three
  outcomes (`Folded` / `NothingToFold` / `Err`) instead of two; the automatic path is insulated by
  keeping its own `Result<(), String>` wrapper, so `chat.rs`/`skill`/`swarm` callers are unaffected.
- **Forced compaction is more expensive per invocation** (it may fold more turns in one pass than a
  threshold-triggered auto fold would), but it is still bounded by `MAX_FOLD_ROUNDS` and the
  summarizer-input bound per round, and it only runs when the owner explicitly asks for it.

## Alternatives considered

- **Keep the 0.40 tail target and add only the no-op notice** — makes the no-op honest but doesn't
  fix the underlying problem: on a claude-code budget the button would still almost never do
  anything, just now saying so instead of pretending to. Rejected — "compact now" must actually
  compact when there is anything left to fold, not just report why it didn't.
- **A smaller fixed byte target for forced compaction (e.g. "always fold down to 4 KB")** — trades
  one magic number tied to `AI_BUDGET_BYTES` (the exact mistake ADR-0014 undid) for a new one that
  re-breaks the same way the next time budgets grow, and gives no reason to prefer 4 KB over any
  other constant. Rejected in favor of `tail_target_bytes: 0`, which has no magic number to drift.
- **Force-fold to zero tail turns too (fold the final turn as well)** — would remove the
  "always leave ≥1 verbatim tail turn" guard `choose_high_water` relies on elsewhere, and a
  reopened idea with no verbatim turn at all has nothing concrete to answer against. Rejected;
  kept the existing tail-turn guard unconditional.

---

> ADRs are immutable once **Accepted**. To change this decision, write a new ADR that supersedes it.
