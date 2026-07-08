//! Auto-compact (docs/adr/0012): a rolling summary of the conversation *head*, folded in a
//! background job so a long discussion keeps fitting a small local model's context budget.
//!
//! The mechanism is a derived, deletable sidecar `vault/<slug>/compacted.md` (see
//! `vault::store::{read,write}_compacted`) whose body summarizes `turns[0..k]` and whose
//! `covered_bytes` frontmatter fingerprints that immutable prefix. Because `conversation.md` is
//! append-only, `turns[0..k]` cannot change from new turns, so the summary never goes stale from
//! appends; the only prefix mutation (`delete_turn` inside the summarized range) breaks the
//! fingerprint, which [`effective_window`] detects and falls back to the full transcript —
//! self-healing with zero index-adjustment code.
//!
//! Two pure helpers ([`effective_window`], [`choose_high_water`]) are shared by the load path
//! (`memory::load`), the meter (`web::routes::ideas`), and the fold job, so "effective size" has
//! exactly one definition. The fold itself ([`run_compaction`]) is the only part that calls the
//! model — under the shared `ai_semaphore` (ADR-0006), once per bounded round.

use std::path::Path;

use chrono::Utc;
use tokio::sync::Semaphore;

use crate::ai::budget::{assemble_context, ContextBudget, ContextInput};
use crate::ai::ollama::ChatMessage;
use crate::ai::LlmBackend;
use crate::domain::{Compacted, CompactedFrontmatter};
use crate::memory::MemoryError;
use crate::vault::store;
use crate::vault::store::split_turns;

/// Compaction's internal byte targets — fixed fractions of ONE budget snapshot (the live
/// [`LlmBackend::context_budget`] at the moment a compaction starts). `memory` sits below `web`
/// in the D4 layering, so it derives the budget from the `LlmBackend` it already holds rather
/// than reading anything from `web`. Snapshotted once per compaction and threaded through every
/// fold round, so a Settings flip mid-fold can never mix targets from two different budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactTargets {
    /// Target size of the verbatim tail `turns[k..n]` after a fold (0.40 of the budget).
    /// Folding advances `k` until the tail is at or under this.
    pub tail_target_bytes: usize,
    /// Hard cap on the summary body (0.30 of the budget) — a derived file may be trimmed.
    pub summary_max_bytes: usize,
    /// Bound on the summarizer *input* (prior summary + the fold slice), 1.00 of the budget —
    /// so the fold call itself stays within one model context.
    pub summarizer_input_bytes: usize,
}

impl CompactTargets {
    /// Derive the targets from a prompt byte budget (the 2/5, 3/10, 1/1 fractions above).
    pub fn for_budget(budget_bytes: usize) -> Self {
        Self {
            tail_target_bytes: budget_bytes * 2 / 5,
            summary_max_bytes: budget_bytes * 3 / 10,
            summarizer_input_bytes: budget_bytes,
        }
    }

    /// Targets for an owner-forced fold (ADR-0016): a zero tail target, so `choose_high_water`
    /// advances until only the final turn stays verbatim (or the summarizer-input bound stops the
    /// round). The auto path's 2/5 tail target exists to avoid *unnecessary* folds; "compact now"
    /// is the owner declaring the fold necessary, so it must not defer to that slack — under a
    /// large dynamic budget (ADR-0014) the 2/5 target made the button a silent no-op.
    pub fn forced(budget_bytes: usize) -> Self {
        Self {
            tail_target_bytes: 0,
            ..Self::for_budget(budget_bytes)
        }
    }
}

/// Max fold rounds per compaction (a cold reopened idea converges in one compaction instead of
/// one-fold-per-turn); each round is its own Ollama call + permit, so this bounds worst-case work.
pub const MAX_FOLD_ROUNDS: usize = 4;

/// The compaction instruction (styled after `EXTRACT_INSTRUCTION`/`CONSOLIDATE_INSTRUCTION`).
/// Fixed `##` headings keep the summary stable, diffable, and coherent to re-fold into.
const COMPACT_INSTRUCTION: &str = "You are compacting the EARLIER part of an ideation \
conversation so it can continue within a tight context budget; the full verbatim transcript is \
preserved elsewhere. Merge the PRIOR SUMMARY (if given, under `## Earlier in this discussion`) \
with the NEW TURNS (under `## Conversation`) into one dense, self-contained running summary. Do \
not invent — compress. A reader must be able to continue from your summary alone. Preserve, under \
these EXACT headings:\n\
- **Decisions** — conclusions reached and the why (rationale), so they are not re-litigated.\n\
- **Open threads** — unresolved questions, next angles still being pushed on.\n\
- **Rejected forks** — directions explicitly abandoned and the reason each was killed.\n\
- **Key facts & constraints** — numbers, scope, hard limits surfaced in discussion.\n\
Do not discard anything from the PRIOR SUMMARY unless the NEW TURNS supersede it. Drop \
pleasantries, restated questions, verbatim phrasing. Output ONLY the four `##` headings with \
dense bullets, under ~1200 tokens. No preamble.";

/// The result of resolving how much of a transcript a `compacted.md` actually covers *right now*
/// — one definition of "effective" for load, meter, and the fold trigger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveWindow {
    /// `Some(k)` when a valid summary covers `turns[0..k]` (feed summary + `turns[k..]`); `None`
    /// when there is no summary, or its fingerprint no longer matches (feed the full transcript).
    pub applied: Option<usize>,
    /// The byte size actually fed to the model: `summary + tail` when applied, else the whole
    /// transcript. This is what the meter and the threshold gate measure.
    pub effective_bytes: usize,
    /// Mirror of `applied` for display ("compacted through turn N"); `None` when not applied.
    pub compacted_through: Option<usize>,
}

/// Σ `turns[0..k]` counted as `trim_end().len() + 1` per turn — the SAME accounting the budget
/// assembler uses for a turn, so `covered_bytes` and the transcript can never disagree. `k` past
/// the end saturates at the full sum (guarded by the caller).
fn prefix_bytes(turns: &[String], k: usize) -> usize {
    turns.iter().take(k).map(|t| t.trim_end().len() + 1).sum()
}

/// Σ `turns[k..]` (the verbatim tail) in the same per-turn accounting.
fn tail_bytes(turns: &[String], k: usize) -> usize {
    turns.iter().skip(k).map(|t| t.trim_end().len() + 1).sum()
}

/// Σ all turns — the effective size when no summary applies.
fn all_bytes(turns: &[String]) -> usize {
    tail_bytes(turns, 0)
}

/// Resolve the effective window for `turns` given an optional `compacted.md`. Pure, O(k): applies
/// the summary only when its `compacted_through` is in range AND `prefix_bytes` still equals the
/// stored `covered_bytes` (the fingerprint). Any prefix mutation, or a missing/short transcript,
/// yields `applied = None` and the full-transcript size — the self-heal.
pub fn effective_window(turns: &[String], compacted: Option<&Compacted>) -> EffectiveWindow {
    match compacted {
        Some(c)
            if c.frontmatter.compacted_through <= turns.len()
                && prefix_bytes(turns, c.frontmatter.compacted_through)
                    == c.frontmatter.covered_bytes =>
        {
            let k = c.frontmatter.compacted_through;
            EffectiveWindow {
                applied: Some(k),
                effective_bytes: c.summary.len() + tail_bytes(turns, k),
                compacted_through: Some(k),
            }
        }
        _ => EffectiveWindow {
            applied: None,
            effective_bytes: all_bytes(turns),
            compacted_through: None,
        },
    }
}

/// Choose the new high-water mark `k_new` (which turns to fold) for one round, starting from
/// `k_old`. Advances forward while (a) the tail is still over `targets.tail_target_bytes`,
/// (b) folding the next turn keeps the summarizer input (`prev_summary + fold slice`) within
/// `targets.summarizer_input_bytes`, and (c) at least one verbatim tail turn remains. Monotonic
/// (`k_new >= k_old`) and never folds a turn the assembler couldn't take — so no fold-slice turn is
/// ever silently dropped. A single turn larger than the input bound (or a single-turn tail) is a
/// no-op (`k_new == k_old`): the tail honestly stays over budget and re-triggers next turn.
pub fn choose_high_water(
    turns: &[String],
    k_old: usize,
    prev_summary: Option<&str>,
    targets: CompactTargets,
) -> usize {
    let n = turns.len();
    if k_old >= n {
        return k_old;
    }
    let prev_len = prev_summary.map_or(0, str::len);
    let mut k = k_old;
    let mut fold_bytes = 0usize;
    while k < n {
        // (c) Always leave ≥1 verbatim tail turn — never fold the final turn.
        if k >= n - 1 {
            break;
        }
        // (a) Stop once the tail already fits the verbatim target — nothing more to fold.
        if tail_bytes(turns, k) <= targets.tail_target_bytes {
            break;
        }
        let turn_len = turns[k].trim_end().len() + 1;
        // (b) Stop before the summarizer input would overflow.
        if prev_len + fold_bytes + turn_len > targets.summarizer_input_bytes {
            break;
        }
        fold_bytes += turn_len;
        k += 1;
    }
    k
}

/// Truncate `s` to at most `max` bytes without splitting a UTF-8 code point.
fn trim_to_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].trim_end().to_string()
}

/// Fold one round for `slug`: read the transcript + prior summary, choose the high-water mark,
/// summarize exactly `turns[k_old..k_new]` (merged with the prior summary) via one Ollama call
/// under a single `ai_semaphore` permit, and write the new `compacted.md`. Returns `Ok(None)` when
/// there is nothing to fold (no model call, no write). Empty model output aborts the round with
/// the previous `compacted.md` left intact. `targets` is the caller's one-per-compaction budget
/// snapshot ([`CompactTargets`]).
pub async fn run_compaction_inner(
    llm: &LlmBackend,
    sem: &Semaphore,
    vault_dir: &Path,
    slug: &str,
    targets: CompactTargets,
) -> Result<Option<Compacted>, MemoryError> {
    let idea = store::read_idea(vault_dir, slug)?;
    let conversation = store::read_conversation(vault_dir, slug)?;
    let turns = split_turns(&conversation);
    let n = turns.len();

    // Validate the prior summary's fingerprint. A stale (mutated-prefix) or absent summary means
    // rebuild from scratch: k_old = 0, no prior summary.
    let prev = store::read_compacted(vault_dir, slug)?;
    let win = effective_window(&turns, prev.as_ref());
    let (k_old, prev_summary): (usize, Option<&str>) = match win.applied {
        Some(k) => (k, prev.as_ref().map(|c| c.summary.as_str())),
        None => (0, None),
    };

    let k_new = choose_high_water(&turns, k_old, prev_summary, targets);
    if k_new == k_old {
        return Ok(None); // nothing summarizable this round — honest no-op
    }

    // Assemble the summarizer input: idea body for grounding + prior summary + ONLY the new fold
    // slice `turns[k_old..k_new]`. `turns[0..k_old]` are represented solely by `prev_summary`, so
    // each turn is folded exactly once (no double-count). The budget carries the idea body on top
    // of the input bound so the chosen slice can never be trimmed by the assembler.
    let fold_budget = targets
        .summarizer_input_bytes
        .saturating_add(idea.body.len())
        .saturating_add(1024);
    let context = assemble_context(
        ContextBudget::new(fold_budget),
        ContextInput {
            idea_body: &idea.body,
            memory: &[],
            summary: prev_summary,
            turns: &turns[k_old..k_new],
        },
    );
    let prompt = format!("{COMPACT_INSTRUCTION}\n\n{}", context.text);

    // One permit around exactly the one call (ADR-0006), released before the write.
    let raw = {
        let _permit = sem
            .acquire()
            .await
            .map_err(|_| MemoryError::SemaphoreClosed)?;
        llm.chat(vec![ChatMessage {
            role: "user".to_string(),
            content: prompt,
        }])
        .await?
    };

    let summary = trim_to_bytes(raw.trim(), targets.summary_max_bytes);
    if summary.is_empty() {
        // Abort with truth intact — the previous compacted.md (if any) is untouched.
        return Err(MemoryError::EmptyCompaction);
    }

    let compacted = Compacted {
        frontmatter: CompactedFrontmatter {
            compacted_through: k_new,
            covered_bytes: prefix_bytes(&turns, k_new),
            turn_count_at_compaction: n,
            model: llm.model(),
            updated: Utc::now(),
        },
        summary,
    };
    store::write_compacted(vault_dir, slug, &compacted)?;
    Ok(Some(compacted))
}

/// What a whole compaction ([`run_compaction`]) actually did — so the manual route can tell an
/// honest no-op apart from a real fold and show the owner a notice instead of nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactOutcome {
    /// At least one round folded and rewrote `compacted.md`.
    Folded,
    /// No round had anything to fold (under threshold, single-turn tail, or already compacted).
    NothingToFold,
}

/// Fold `slug` toward the tail target. When `force` is false this first applies the settings gate
/// (auto-compact on, and effective size at/over `compact_threshold` of the budget) and no-ops
/// otherwise — so no threshold logic sits on the request hot path. When `force` is true the gate
/// is skipped and the fold runs at [`CompactTargets::forced`] (zero tail target — fold everything
/// except the final turn, ADR-0016). Loops up to [`MAX_FOLD_ROUNDS`] so a cold/long transcript
/// converges in one compaction. `conversation.md` is never written; only `compacted.md` is
/// (re)written. Returns a human-readable error string for the job indicator.
pub async fn run_compaction(
    llm: &LlmBackend,
    sem: &Semaphore,
    vault_dir: &Path,
    slug: &str,
    force: bool,
) -> Result<CompactOutcome, String> {
    // Snapshot the live budget ONCE per compaction and derive every target from it — the gate
    // and all fold rounds see the same numbers even if the Settings page flips mid-fold. (A
    // budget change between compactions just means one convergence burst at the new targets;
    // the covered_bytes fingerprint is budget-independent, so nothing goes stale.)
    let budget_bytes = llm.context_budget().max_bytes;
    if !force && !over_threshold(llm, vault_dir, slug, budget_bytes).map_err(|e| e.to_string())? {
        return Ok(CompactOutcome::NothingToFold);
    }
    let targets = if force {
        CompactTargets::forced(budget_bytes)
    } else {
        CompactTargets::for_budget(budget_bytes)
    };
    let mut rounds = 0usize;
    for _ in 0..MAX_FOLD_ROUNDS {
        match run_compaction_inner(llm, sem, vault_dir, slug, targets).await {
            Ok(Some(c)) => {
                rounds += 1;
                tracing::info!(
                    slug,
                    force,
                    round = rounds,
                    compacted_through = c.frontmatter.compacted_through,
                    "compaction folded a round"
                );
            }
            Ok(None) => break, // converged / nothing to fold
            Err(e) => return Err(e.to_string()),
        }
    }
    if rounds == 0 {
        tracing::debug!(slug, force, "compaction had nothing to fold");
        return Ok(CompactOutcome::NothingToFold);
    }
    Ok(CompactOutcome::Folded)
}

/// The auto path's threshold gate: auto-compact enabled AND the *effective* size (summary + tail,
/// not raw `conversation.len()`) is at or over `compact_threshold * budget_bytes` (the caller's
/// one budget snapshot). Cheap: one small file read + an O(k) sum.
fn over_threshold(
    llm: &LlmBackend,
    vault_dir: &Path,
    slug: &str,
    budget_bytes: usize,
) -> Result<bool, MemoryError> {
    let settings = llm.settings();
    if !settings.auto_compact {
        return Ok(false);
    }
    let conversation = store::read_conversation(vault_dir, slug)?;
    let turns = split_turns(&conversation);
    let compacted = store::read_compacted(vault_dir, slug)?;
    let win = effective_window(&turns, compacted.as_ref());
    let trigger = (settings.compact_threshold * budget_bytes as f32) as usize;
    Ok(win.effective_bytes >= trigger)
}

/// The auto (chat-phase-0) entry point: the settings-gated fold. Best-effort at the call site —
/// the chat job logs any error and proceeds with fallback context (docs/adr/0012).
pub async fn maybe_run_compaction(
    llm: &LlmBackend,
    sem: &Semaphore,
    vault_dir: &Path,
    slug: &str,
) -> Result<(), String> {
    run_compaction(llm, sem, vault_dir, slug, false)
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pre-dynamic-budget fixed targets (16 KiB base) — the regression guard: with this
    /// budget every fold decision must be byte-identical to the old constants.
    fn targets_16k() -> CompactTargets {
        CompactTargets::for_budget(16 * 1024)
    }

    fn turns_of(sizes: &[usize]) -> Vec<String> {
        // Each "turn" is a heading line + a body of `size` filler bytes; split_turns accounting is
        // `trim_end().len() + 1`, so we build turns whose trimmed length is predictable.
        sizes
            .iter()
            .enumerate()
            .map(|(i, &size)| format!("## user\n{}", "x".repeat(size.saturating_sub(i % 2))))
            .collect()
    }

    fn compacted(k: usize, covered: usize, summary: &str) -> Compacted {
        Compacted {
            frontmatter: CompactedFrontmatter {
                compacted_through: k,
                covered_bytes: covered,
                turn_count_at_compaction: k,
                model: "test".into(),
                updated: Utc::now(),
            },
            summary: summary.into(),
        }
    }

    #[test]
    fn effective_window_none_compacted_is_all_bytes() {
        let turns = turns_of(&[100, 100, 100]);
        let win = effective_window(&turns, None);
        assert_eq!(win.applied, None);
        assert_eq!(win.effective_bytes, all_bytes(&turns));
        assert_eq!(win.compacted_through, None);
    }

    #[test]
    fn effective_window_matching_fingerprint_applies_summary() {
        let turns = turns_of(&[100, 100, 100, 100]);
        let covered = prefix_bytes(&turns, 2);
        let c = compacted(2, covered, "SUMMARY-BODY");
        let win = effective_window(&turns, Some(&c));
        assert_eq!(win.applied, Some(2));
        assert_eq!(win.compacted_through, Some(2));
        // Effective = summary length + verbatim tail (turns[2..]).
        assert_eq!(
            win.effective_bytes,
            "SUMMARY-BODY".len() + tail_bytes(&turns, 2)
        );
    }

    #[test]
    fn effective_window_mismatched_fingerprint_falls_back_to_full() {
        let turns = turns_of(&[100, 100, 100, 100]);
        // Wrong covered_bytes (as if the prefix was mutated by a head delete_turn).
        let c = compacted(2, prefix_bytes(&turns, 2) + 7, "SUMMARY");
        let win = effective_window(&turns, Some(&c));
        assert_eq!(win.applied, None);
        assert_eq!(win.effective_bytes, all_bytes(&turns));
    }

    #[test]
    fn effective_window_out_of_range_k_is_guarded() {
        let turns = turns_of(&[100, 100]);
        let c = compacted(9, prefix_bytes(&turns, 2), "S");
        let win = effective_window(&turns, Some(&c));
        assert_eq!(
            win.applied, None,
            "compacted_through > len is never applied"
        );
    }

    #[test]
    fn for_budget_derives_the_fixed_fractions() {
        let t = CompactTargets::for_budget(16 * 1024);
        assert_eq!(t.tail_target_bytes, 16 * 1024 * 2 / 5);
        assert_eq!(t.summary_max_bytes, 16 * 1024 * 3 / 10);
        assert_eq!(t.summarizer_input_bytes, 16 * 1024);

        // Targets scale with the live budget (the whole point of dynamic budgets).
        let big = CompactTargets::for_budget(64 * 1024);
        assert_eq!(big.tail_target_bytes, 64 * 1024 * 2 / 5);
        assert_eq!(big.summary_max_bytes, 64 * 1024 * 3 / 10);
        assert_eq!(big.summarizer_input_bytes, 64 * 1024);
    }

    #[test]
    fn forced_targets_zero_the_tail_and_keep_the_other_bounds() {
        let budget = 400 * 1024;
        let forced = CompactTargets::forced(budget);
        let auto = CompactTargets::for_budget(budget);
        assert_eq!(forced.tail_target_bytes, 0);
        assert_eq!(forced.summary_max_bytes, auto.summary_max_bytes);
        assert_eq!(forced.summarizer_input_bytes, auto.summarizer_input_bytes);
    }

    #[test]
    fn forced_targets_fold_everything_but_the_final_turn() {
        // A conversation far under the auto tail target (the "compact now on a big budget" case,
        // ADR-0016): the auto targets are a no-op, the forced targets fold all but the last turn.
        let budget = 400 * 1024;
        let turns = turns_of(&[400, 400, 400, 400, 400]);
        assert_eq!(
            choose_high_water(&turns, 0, None, CompactTargets::for_budget(budget)),
            0,
            "auto targets: under the tail target ⇒ nothing to fold"
        );
        assert_eq!(
            choose_high_water(&turns, 0, None, CompactTargets::forced(budget)),
            turns.len() - 1,
            "forced targets: everything except the final turn"
        );
    }

    #[test]
    fn forced_targets_on_a_single_turn_are_a_no_op() {
        // One turn ⇒ nothing foldable even when forced (guard (c): the final turn stays verbatim),
        // so run_compaction reports NothingToFold and the route shows the notice.
        let turns = turns_of(&[400]);
        assert_eq!(
            choose_high_water(&turns, 0, None, CompactTargets::forced(400 * 1024)),
            0
        );
    }

    #[test]
    fn a_larger_budget_folds_less() {
        // A transcript over the 16 KiB tail target but under a 64 KiB one: the small budget
        // folds, the big budget is a no-op.
        let per = 400usize;
        let small = targets_16k();
        let count = (small.tail_target_bytes / per) + 6;
        let turns = turns_of(&vec![per; count]);
        assert!(choose_high_water(&turns, 0, None, small) > 0);
        assert_eq!(
            choose_high_water(&turns, 0, None, CompactTargets::for_budget(64 * 1024)),
            0,
            "the same transcript fits a larger budget's tail target"
        );
    }

    #[test]
    fn choose_high_water_is_monotonic_and_leaves_a_tail() {
        // Many small turns, total well over the tail target — folding must advance but keep ≥1 tail.
        let t = targets_16k();
        let per = 400usize;
        let count = (t.tail_target_bytes / per) + 6;
        let turns = turns_of(&vec![per; count]);
        let k = choose_high_water(&turns, 0, None, t);
        assert!(k < turns.len(), "leaves ≥1 verbatim tail turn");
        assert!(k > 0, "advanced past the oldest turns");
        // The folded slice fits the summarizer input bound (no silent loss).
        assert!(prefix_bytes(&turns, k) <= t.summarizer_input_bytes);
        // And the resulting tail is at/under target (or one turn shy of it).
        assert!(tail_bytes(&turns, k) <= t.tail_target_bytes + per);
    }

    #[test]
    fn choose_high_water_single_giant_turn_is_a_no_op() {
        // One turn larger than the summarizer input, plus a small tail turn: the giant can't be
        // folded (would overflow the input), so k stays at k_old and the tail stays over budget.
        let t = targets_16k();
        let turns = vec![
            format!("## user\n{}", "x".repeat(t.summarizer_input_bytes + 500)),
            "## user\nsmall tail".to_string(),
        ];
        let k = choose_high_water(&turns, 0, None, t);
        assert_eq!(k, 0, "a single over-budget turn is never folded");
    }

    #[test]
    fn choose_high_water_never_folds_the_final_turn() {
        let t = targets_16k();
        let turns = turns_of(&[50, 50]); // tiny, under target
        let k = choose_high_water(&turns, 0, None, t);
        assert_eq!(k, 0, "under target ⇒ nothing to fold");
        // Even a big two-turn transcript keeps the last turn verbatim.
        let big = turns_of(&[t.tail_target_bytes, 200]);
        let k = choose_high_water(&big, 0, None, t);
        assert!(k < big.len());
    }

    #[test]
    fn prefix_and_tail_bytes_partition_the_transcript() {
        let turns = turns_of(&[10, 20, 30, 40]);
        for k in 0..=turns.len() {
            assert_eq!(
                prefix_bytes(&turns, k) + tail_bytes(&turns, k),
                all_bytes(&turns)
            );
        }
    }

    #[test]
    fn trim_to_bytes_respects_char_boundaries() {
        let s = "héllo wörld"; // multi-byte chars
        let out = trim_to_bytes(s, 3);
        assert!(s.starts_with(&out));
        assert!(out.len() <= 3);
        // No panic / no split char: round-trips as valid UTF-8 (guaranteed by &str slicing).
        let _ = out.chars().count();
    }
}
