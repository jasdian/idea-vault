//! Auto-compact engine tests (docs/adr/0012), driven against the mock Ollama — never a live model.
//! Covers the fold slice / no-double-count / empty-output-abort invariants, the delete_turn
//! fingerprint edges (self-heal), reopen reuse, and that Store distils the FULL transcript and
//! never touches `compacted.md`.

mod support;

use chrono::{TimeZone, Utc};
use idea_vault::ai::budget::ContextBudget;
use idea_vault::ai::{LlmBackend, OllamaClient};
use idea_vault::domain::{Idea, IdeaFrontmatter, IdeaState};
use idea_vault::memory::compact;
use idea_vault::memory::compact::CompactTargets;
use idea_vault::vault::store;
use support::{spawn, spawn_sequence, spawn_with_context_length, ChatScript, MockOllama};
use tokio::sync::Semaphore;

/// The pre-dynamic-budget fixed targets (16 KiB base): with these, every fold decision below is
/// byte-identical to the old constants — the regression guard for the dynamic-budget change.
fn targets_16k() -> CompactTargets {
    CompactTargets::for_budget(16 * 1024)
}

/// Per-turn byte accounting used for `covered_bytes` (same as the engine/assembler).
fn prefix_bytes(turns: &[String], k: usize) -> usize {
    turns.iter().take(k).map(|t| t.trim_end().len() + 1).sum()
}

fn seed(vault: &std::path::Path, slug: &str, state: IdeaState) {
    store::write_idea(
        vault,
        &Idea {
            frontmatter: IdeaFrontmatter {
                title: "Compactable".into(),
                slug: slug.into(),
                state,
                tags: vec![],
                created: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                updated: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
            },
            body: "The idea body.\n".into(),
        },
    )
    .unwrap();
}

/// Append `n` uniquely-marked ~1 KB user turns starting at marker `from`. Turn sizes vary (the
/// realistic case) so a head deletion measurably changes the prefix byte count — a uniform-size
/// transcript is a known blind spot of a byte-count fingerprint, not the case worth testing.
fn append_marked(vault: &std::path::Path, slug: &str, from: usize, n: usize) {
    for i in from..from + n {
        let content = format!("MARKER{i} {}", "x".repeat(1000 + i * 40));
        store::append_turn(vault, slug, "user", &content).unwrap();
    }
}

fn backend(mock: &MockOllama) -> LlmBackend {
    let ollama = OllamaClient::new(mock.url.clone(), "llama3.2".to_string()).unwrap();
    LlmBackend::ollama_only(ollama)
}

#[tokio::test]
async fn inner_folds_exactly_the_head_slice_and_writes_a_correct_fingerprint() {
    let mock = spawn(
        &["llama3.2"],
        ChatScript::Tokens(vec!["## Decisions\n".into(), "- folded head".into()]),
    )
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let vault = tmp.path();
    seed(vault, "i", IdeaState::InDiscussion);
    append_marked(vault, "i", 0, 10); // ~10 KB transcript, well over the 0.40 tail target

    let sem = Semaphore::new(2);
    let out = compact::run_compaction_inner(&backend(&mock), &sem, vault, "i", targets_16k())
        .await
        .unwrap()
        .expect("a fold happened");

    let convo = store::read_conversation(vault, "i").unwrap();
    let turns = store::split_turns(&convo);
    let k = out.frontmatter.compacted_through;
    assert!(k > 0 && k < turns.len(), "folded some head, left a tail");
    assert_eq!(
        out.frontmatter.covered_bytes,
        prefix_bytes(&turns, k),
        "covered_bytes fingerprints prefix turns[0..k]"
    );
    assert_eq!(out.frontmatter.turn_count_at_compaction, turns.len());
    assert_eq!(out.summary, "## Decisions\n- folded head");

    // The one fold request carried the head turns, not the verbatim tail.
    let body = &mock.chat_bodies()[0];
    assert!(body.contains("MARKER0"), "head turn 0 was folded");
    assert!(
        !body.contains(&format!("MARKER{}", turns.len() - 1)),
        "the verbatim tail turn was NOT sent to the summarizer"
    );
}

#[tokio::test]
async fn second_round_reuses_the_prior_summary_and_never_refolds_the_head() {
    // Round 1 folds turns[0..k]; then more turns arrive and round 2 folds only the NEW slice,
    // carrying the head forward as the prior summary — turn 0 is never re-fed (no double-count).
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            ChatScript::Tokens(vec!["## Decisions\n- round one".into()]),
            ChatScript::Tokens(vec!["## Decisions\n- round two".into()]),
        ],
    )
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let vault = tmp.path();
    seed(vault, "i", IdeaState::InDiscussion);
    append_marked(vault, "i", 0, 10);

    let sem = Semaphore::new(2);
    let r1 = compact::run_compaction_inner(&backend(&mock), &sem, vault, "i", targets_16k())
        .await
        .unwrap()
        .unwrap();

    // Append more head-growing turns so round 2 has a new slice to fold.
    append_marked(vault, "i", 10, 8);
    let r2 = compact::run_compaction_inner(&backend(&mock), &sem, vault, "i", targets_16k())
        .await
        .unwrap()
        .unwrap();

    assert!(r2.frontmatter.compacted_through > r1.frontmatter.compacted_through);
    let body2 = &mock.chat_bodies()[1];
    assert!(
        body2.contains("Earlier in this discussion"),
        "round 2 carries the prior summary forward"
    );
    assert!(
        body2.contains("round one"),
        "prior summary body is included"
    );
    assert!(
        !body2.contains("MARKER0"),
        "turns[0..k_old] are represented only by the summary, never re-fed"
    );
}

#[tokio::test]
async fn empty_model_output_aborts_and_leaves_the_previous_summary_intact() {
    // First fold succeeds; then a fold whose model output is empty must abort with the earlier
    // compacted.md untouched (mirrors extract_and_store's truth-intact-on-failure).
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            ChatScript::Tokens(vec!["## Decisions\n- kept".into()]),
            ChatScript::Tokens(vec![]), // empty → abort
        ],
    )
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let vault = tmp.path();
    seed(vault, "i", IdeaState::InDiscussion);
    append_marked(vault, "i", 0, 10);

    let sem = Semaphore::new(2);
    let first = compact::run_compaction_inner(&backend(&mock), &sem, vault, "i", targets_16k())
        .await
        .unwrap()
        .unwrap();
    append_marked(vault, "i", 10, 8);
    let err = compact::run_compaction_inner(&backend(&mock), &sem, vault, "i", targets_16k())
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        idea_vault::memory::MemoryError::EmptyCompaction
    ));
    // The previous summary is still on disk, unchanged.
    let now = store::read_compacted(vault, "i").unwrap().unwrap();
    assert_eq!(
        now, first,
        "aborted fold left the prior compacted.md intact"
    );
}

#[tokio::test]
async fn delete_turn_after_the_fold_boundary_preserves_the_fingerprint() {
    let mock = spawn(
        &["llama3.2"],
        ChatScript::Tokens(vec!["## Decisions\n- x".into()]),
    )
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let vault = tmp.path();
    seed(vault, "i", IdeaState::InDiscussion);
    append_marked(vault, "i", 0, 10);
    let sem = Semaphore::new(2);
    let folded = compact::run_compaction_inner(&backend(&mock), &sem, vault, "i", targets_16k())
        .await
        .unwrap()
        .unwrap();
    let k = folded.frontmatter.compacted_through;

    // Delete a turn in the verbatim TAIL (index >= k): the folded prefix is untouched.
    let turns_before = store::split_turns(&store::read_conversation(vault, "i").unwrap());
    assert!(store::delete_turn(vault, "i", turns_before.len() - 1).unwrap());

    let turns = store::split_turns(&store::read_conversation(vault, "i").unwrap());
    let compacted = store::read_compacted(vault, "i").unwrap();
    let win = compact::effective_window(&turns, compacted.as_ref());
    assert_eq!(win.applied, Some(k), "tail delete keeps the summary valid");
}

#[tokio::test]
async fn delete_turn_inside_the_fold_breaks_the_fingerprint_and_rebuilds_from_zero() {
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            ChatScript::Tokens(vec!["## Decisions\n- first".into()]),
            ChatScript::Tokens(vec!["## Decisions\n- rebuilt".into()]),
        ],
    )
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let vault = tmp.path();
    seed(vault, "i", IdeaState::InDiscussion);
    append_marked(vault, "i", 0, 10);
    let sem = Semaphore::new(2);
    let folded = compact::run_compaction_inner(&backend(&mock), &sem, vault, "i", targets_16k())
        .await
        .unwrap()
        .unwrap();
    assert!(folded.frontmatter.compacted_through > 0);

    // Delete a HEAD turn (index 0 < k): the prefix bytes change → fingerprint mismatch → fallback.
    assert!(store::delete_turn(vault, "i", 0).unwrap());
    let turns = store::split_turns(&store::read_conversation(vault, "i").unwrap());
    let compacted = store::read_compacted(vault, "i").unwrap();
    assert_eq!(
        compact::effective_window(&turns, compacted.as_ref()).applied,
        None,
        "head delete invalidates the summary → full-transcript fallback"
    );

    // The next compaction rebuilds from k_old = 0 (the stale summary is discarded, not extended).
    let rebuilt = compact::run_compaction_inner(&backend(&mock), &sem, vault, "i", targets_16k())
        .await
        .unwrap()
        .unwrap();
    // The stale round-1 summary body ("- first") must NOT reappear in the rebuild prompt — the
    // rebuild starts from k_old = 0 with no prior summary block (the instruction text mentions the
    // header generically, so we discriminate on the summary CONTENT instead).
    let body = &mock.chat_bodies()[1];
    assert!(
        !body.contains("- first"),
        "rebuild discards the stale prior summary (k_old = 0, no prior summary block)"
    );
    // The freshly written fingerprint matches the CURRENT transcript again.
    assert_eq!(
        rebuilt.frontmatter.covered_bytes,
        prefix_bytes(&turns, rebuilt.frontmatter.compacted_through)
    );
}

#[tokio::test]
async fn store_reads_the_full_transcript_and_never_touches_compacted_md() {
    // Fold first, then Store: extract_and_store must distil the FULL verbatim transcript (its
    // request contains the folded head turn), and must leave compacted.md byte-for-byte intact.
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            ChatScript::Tokens(vec!["## Decisions\n- folded".into()]), // the fold
            ChatScript::Tokens(vec!["Consolidated body.".into()]),     // store: consolidate
            ChatScript::Tokens(vec!["FACT: A\nbody\n".into()]),        // store: extract
        ],
    )
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let vault = tmp.path();
    seed(vault, "i", IdeaState::InDiscussion);
    append_marked(vault, "i", 0, 10);
    let sem = Semaphore::new(2);
    let llm = backend(&mock);

    compact::run_compaction_inner(&llm, &sem, vault, "i", targets_16k())
        .await
        .unwrap()
        .unwrap();
    let compacted_before = std::fs::read_to_string(vault.join("i/compacted.md")).unwrap();

    idea_vault::memory::extract::extract_and_store(
        &llm,
        &sem,
        vault,
        "i",
        ContextBudget::new(16 * 1024),
    )
    .await
    .unwrap();

    // Store's consolidate call (request index 1) saw the folded head verbatim (full transcript).
    let store_body = &mock.chat_bodies()[1];
    assert!(
        store_body.contains("MARKER0"),
        "Store distils the full transcript, not the rolling summary"
    );
    let compacted_after = std::fs::read_to_string(vault.join("i/compacted.md")).unwrap();
    assert_eq!(
        compacted_before, compacted_after,
        "Store never rewrites or deletes compacted.md"
    );
}

#[tokio::test]
async fn a_big_native_context_window_raises_the_threshold_so_no_fold_fires() {
    // ~15 KB transcript: over the 0.80 × 16 KiB fallback trigger, but far under the trigger once
    // /api/show advertises a 32k-token window (budget 64 KiB) — the dynamic budget in action.
    let mock = spawn_with_context_length(
        &["llama3.2"],
        ChatScript::Tokens(vec!["## Decisions\n- should never be asked".into()]),
        32_768,
    )
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let vault = tmp.path();
    seed(vault, "i", IdeaState::InDiscussion);
    append_marked(vault, "i", 0, 12);

    let sem = Semaphore::new(2);
    let llm = backend(&mock);
    llm.refresh_ollama_ctx().await; // the boot-time cache warm
    compact::run_compaction(&llm, &sem, vault, "i", false)
        .await
        .unwrap();

    assert!(
        store::read_compacted(vault, "i").unwrap().is_none(),
        "under the 32k-window threshold, the auto path must not fold"
    );
    assert!(mock.chat_bodies().is_empty(), "no model call was made");
}

#[tokio::test]
async fn an_unanswered_api_show_falls_back_to_the_16k_budget_and_still_folds() {
    // The plain mock 404s /api/show: the budget stays at the pre-dynamic 16 KiB fallback, so the
    // same ~15 KB transcript trips the 0.80 threshold and the auto path folds as before.
    let mock = spawn(
        &["llama3.2"],
        ChatScript::Tokens(vec!["## Decisions\n- folded at the fallback budget".into()]),
    )
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let vault = tmp.path();
    seed(vault, "i", IdeaState::InDiscussion);
    append_marked(vault, "i", 0, 12);

    let sem = Semaphore::new(2);
    let llm = backend(&mock);
    llm.refresh_ollama_ctx().await; // 404 → nothing cached → fallback budget
    compact::run_compaction(&llm, &sem, vault, "i", false)
        .await
        .unwrap();

    let compacted = store::read_compacted(vault, "i").unwrap().unwrap();
    assert!(compacted.frontmatter.compacted_through > 0);
    // The chat call carried num_ctx at the fallback window (the silent-truncation fix).
    let body = &mock.chat_bodies()[0];
    assert!(
        body.contains("\"num_ctx\":8192"),
        "fold call sends the fallback num_ctx: {body}"
    );
}
