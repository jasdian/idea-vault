//! Web handler tests for R4 store / R5 reopen (D12/D13): the remaining state-machine
//! transitions through the real router — store→Stored writes consolidation + memory,
//! reopen→Reopened loads context truth-idempotently. Mock Ollama only.

mod support;

use axum::http::StatusCode;
use chrono::{TimeZone, Utc};
use idea_vault::domain::{Idea, IdeaFrontmatter, IdeaState};
use idea_vault::vault::store;
use support::web::{poll_until, post_form, test_state_with_ollama};
use support::{spawn_sequence, ChatScript};

fn seed(vault: &std::path::Path, state: IdeaState, with_turns: bool) {
    store::write_idea(
        vault,
        &Idea {
            frontmatter: IdeaFrontmatter {
                title: "Vaulted".into(),
                slug: "vaulted".into(),
                state,
                tags: vec![],
                created: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                updated: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
            },
            body: "Original statement.\n".into(),
        },
    )
    .unwrap();
    if with_turns {
        store::append_turn(vault, "vaulted", "user", "dig in").unwrap();
        store::append_turn(vault, "vaulted", "assistant", "dug").unwrap();
    }
}

fn tokens(text: &str) -> ChatScript {
    ChatScript::Tokens(vec![text.to_string()])
}

#[tokio::test]
async fn store_consolidates_extracts_and_lands_stored() {
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            tokens("Consolidated best statement."),
            tokens("FACT: Durable point\nThe conclusion body.\n"),
        ],
    )
    .await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::InDiscussion, true);
    let convo_before = store::read_conversation(&vault_dir, "vaulted").unwrap();

    let (status, body) = post_form(state.clone(), "/idea/vaulted/store", "").await;
    assert_eq!(status, StatusCode::OK);
    // A background job (ADR-0010): the immediate response is the transcript with the thinking
    // indicator, not the stored view.
    assert!(body.contains("foil-pending"), "indicator shows immediately");

    // The /pending poll delivers the stored partial once the job lands: consolidated body +
    // reopen affordance + the OOB subhead badge flip (the badge lives outside #discussion).
    let body = poll_until(
        state,
        "/idea/vaulted/pending",
        "Consolidated best statement.",
    )
    .await;
    assert!(body.contains("hx-post=\"/idea/vaulted/reopen\""));
    assert!(body.contains("state--stored") && body.contains("hx-swap-oob"));

    // Truth: state=stored in frontmatter, memory on disk, conversation untouched.
    let idea = store::read_idea(&vault_dir, "vaulted").unwrap();
    assert_eq!(idea.frontmatter.state, IdeaState::Stored);
    assert_eq!(idea.body, "Consolidated best statement.\n");
    let facts = store::read_memory_facts(&vault_dir, "vaulted").unwrap();
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].frontmatter.slug, "durable-point");
    assert!(vault_dir.join("vaulted/MEMORY.md").is_file());
    assert_eq!(
        store::read_conversation(&vault_dir, "vaulted").unwrap(),
        convo_before,
        "storing never touches the transcript"
    );
}

#[tokio::test]
async fn store_guards_draft_no_turns_and_already_stored() {
    // Draft → 400.
    let mock = spawn_sequence(&["llama3.2"], vec![]).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::Draft, false);
    let (status, _) = post_form(state, "/idea/vaulted/store", "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // InDiscussion with zero turns → 400 (D9 guard).
    let mock = spawn_sequence(&["llama3.2"], vec![]).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::InDiscussion, false);
    let (status, _) = post_form(state, "/idea/vaulted/store", "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Already stored → 400, and no second extraction happened (no chat calls at all).
    let mock = spawn_sequence(&["llama3.2"], vec![]).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::Stored, true);
    let (status, _) = post_form(state, "/idea/vaulted/store", "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(mock.chat_bodies().is_empty());
}

#[tokio::test]
async fn store_with_ai_down_surfaces_a_job_error_and_truth_untouched() {
    let (state, vault_dir) = support::web::test_state(); // refused Ollama
    seed(&vault_dir, IdeaState::InDiscussion, true);
    let idea_before = store::read_idea(&vault_dir, "vaulted").unwrap();

    // The job pattern: the POST succeeds (indicator shown), the AI failure surfaces on the next
    // poll as the consumed-once error block — same contract as chat/skill/swarm.
    let (status, body) = post_form(state.clone(), "/idea/vaulted/store", "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("foil-pending"));
    poll_until(
        state,
        "/idea/vaulted/pending",
        "The foil could not respond.",
    )
    .await;

    assert_eq!(
        store::read_idea(&vault_dir, "vaulted").unwrap(),
        idea_before
    );
    assert!(store::read_memory_facts(&vault_dir, "vaulted")
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn reopen_flips_state_loads_context_and_returns_discussion() {
    // Store first so memory exists, then reopen.
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            tokens("Stored statement."),
            tokens("FACT: Key point\nRemember this.\n"),
        ],
    )
    .await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::InDiscussion, true);
    let (status, _) = post_form(state.clone(), "/idea/vaulted/store", "").await;
    assert_eq!(status, StatusCode::OK);
    poll_until(state.clone(), "/idea/vaulted/pending", "Stored statement.").await;
    let body_before = store::read_idea(&vault_dir, "vaulted").unwrap().body;
    let facts_before = store::read_memory_facts(&vault_dir, "vaulted").unwrap();

    let (status, body) = post_form(state, "/idea/vaulted/reopen", "").await;
    assert_eq!(status, StatusCode::OK);
    // The discussion pane returns, transcript intact, compose live (mock is Available), store
    // control back, and the OOB subhead badge flipped to reopened.
    assert!(body.contains("hx-post=\"/idea/vaulted/chat\""));
    assert!(body.contains("dig in"), "transcript rendered");
    assert!(body.contains("/idea/vaulted/store"), "store control back");
    assert!(body.contains("state--reopened") && body.contains("hx-swap-oob"));

    // D13 truth-idempotence: only the state flipped; body and memory untouched.
    let idea = store::read_idea(&vault_dir, "vaulted").unwrap();
    assert_eq!(idea.frontmatter.state, IdeaState::Reopened);
    assert_eq!(idea.body, body_before);
    assert_eq!(
        store::read_memory_facts(&vault_dir, "vaulted").unwrap(),
        facts_before
    );
}

#[tokio::test]
async fn restore_from_reopened_merges_memory_without_turn_guard() {
    // Full loop: store → reopen → re-store. The Reopened→Stored row of D9 has no turn guard
    // and must merge (not drop) memory.
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            tokens("v1."),
            tokens("FACT: First point\nBody one.\n"),
            tokens("v2 after reopen."),
            tokens("FACT: First point\nDuplicate.\nFACT: Second point\nBody two.\n"),
        ],
    )
    .await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::InDiscussion, true);

    let (s, _) = post_form(state.clone(), "/idea/vaulted/store", "").await;
    assert_eq!(s, StatusCode::OK);
    poll_until(state.clone(), "/idea/vaulted/pending", "v1.").await;
    let (s, _) = post_form(state.clone(), "/idea/vaulted/reopen", "").await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = post_form(state.clone(), "/idea/vaulted/store", "").await;
    assert_eq!(s, StatusCode::OK, "Reopened→Stored needs no new turns");
    poll_until(state, "/idea/vaulted/pending", "v2 after reopen.").await;

    let idea = store::read_idea(&vault_dir, "vaulted").unwrap();
    assert_eq!(idea.frontmatter.state, IdeaState::Stored);
    assert_eq!(idea.body, "v2 after reopen.\n");
    let facts = store::read_memory_facts(&vault_dir, "vaulted").unwrap();
    assert_eq!(facts.len(), 2, "memory merged, duplicate deduped");
    assert_eq!(
        facts[0].body, "Body one.\n",
        "existing fact never rewritten"
    );
}

#[tokio::test]
async fn store_on_missing_idea_is_404() {
    let mock = spawn_sequence(&["llama3.2"], vec![]).await;
    let (state, _vault_dir) = test_state_with_ollama(&mock.url, 1);
    let (status, _) = post_form(state, "/idea/ghost/store", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn reopen_guards_non_stored_states_and_missing() {
    let mock = spawn_sequence(&["llama3.2"], vec![]).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::InDiscussion, true);

    let (status, _) = post_form(state.clone(), "/idea/vaulted/reopen", "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = post_form(state, "/idea/ghost/reopen", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
