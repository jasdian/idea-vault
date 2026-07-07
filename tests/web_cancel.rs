//! Web handler tests for cancelling a running background job and for the live per-step progress
//! note surfaced through `/pending`. Mock Ollama only.
//!
//! Cancel aborts the detached task (dropping the in-flight model future), so nothing partial is
//! persisted and the slot clears — a follow-up action on the same idea is accepted again. The
//! progress note is advanced by the orchestrators (swarm reports "swarm · attacking k/N: <angle>")
//! and rendered in the "thinking" indicator.

mod support;

use axum::http::StatusCode;
use chrono::{TimeZone, Utc};
use idea_vault::domain::{Idea, IdeaFrontmatter, IdeaState};
use idea_vault::vault::store;
use support::web::{post_form, test_state_with_ollama};
use support::{spawn, ChatScript};

fn seed(vault: &std::path::Path, state: IdeaState) {
    store::write_idea(
        vault,
        &Idea {
            frontmatter: IdeaFrontmatter {
                title: "Cancellable".into(),
                slug: "cancellable".into(),
                state,
                tags: vec![],
                created: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                updated: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
            },
            body: "The idea body.\n".into(),
        },
    )
    .unwrap();
    store::append_turn(vault, "cancellable", "user", "attack it").unwrap();
}

#[tokio::test]
async fn cancel_aborts_a_running_chat_and_the_slot_is_reclaimed() {
    // A stalling model keeps the chat job running (it never sends `{done}`), so it is cancellable.
    let mock = spawn(&["llama3.2"], ChatScript::StallAfter(1)).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::InDiscussion);

    let (status, _) = post_form(state.clone(), "/idea/cancellable/chat", "message=hello").await;
    assert_eq!(status, StatusCode::OK);
    // Wait until the background job is visibly running (the indicator is present).
    support::web::poll_until(state.clone(), "/idea/cancellable/pending", "foil-pending").await;

    // Cancel: the indicator is gone and the transcript is final.
    let (status, body) = post_form(state.clone(), "/idea/cancellable/cancel", "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !body.contains("foil-pending"),
        "indicator gone after cancel"
    );

    // Nothing partial persisted: the up-front user turn remains, no assistant turn was written.
    let convo = store::read_conversation(&vault_dir, "cancellable").unwrap();
    assert!(convo.contains("## user\nhello"));
    assert!(
        !convo.contains("## assistant"),
        "no partial reply persisted"
    );

    // The slot cleared, so a second chat is accepted (it appends a fresh user turn up front rather
    // than re-showing a busy state).
    let (status, _) = post_form(state, "/idea/cancellable/chat", "message=again").await;
    assert_eq!(status, StatusCode::OK);
    let convo = store::read_conversation(&vault_dir, "cancellable").unwrap();
    assert_eq!(
        convo.matches("## user\n").count(),
        3,
        "seed + hello + again — the slot was reclaimable after cancel"
    );
}

#[tokio::test]
async fn cancel_of_a_running_swarm_persists_nothing() {
    let mock = spawn(&["llama3.2"], ChatScript::StallAfter(1)).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 2);
    seed(&vault_dir, IdeaState::InDiscussion);
    let convo_before = store::read_conversation(&vault_dir, "cancellable").unwrap();

    let (status, _) = post_form(state.clone(), "/idea/cancellable/swarm", "").await;
    assert_eq!(status, StatusCode::OK);
    support::web::poll_until(state.clone(), "/idea/cancellable/pending", "foil-pending").await;

    let (status, _) = post_form(state.clone(), "/idea/cancellable/cancel", "").await;
    assert_eq!(status, StatusCode::OK);

    // The swarm persists only its converged synthesis, and only after everything completes — so a
    // mid-run cancel leaves the conversation exactly as it was.
    assert_eq!(
        store::read_conversation(&vault_dir, "cancellable").unwrap(),
        convo_before
    );
    assert!(!convo_before.contains("## assistant (swarm)"));
}

#[tokio::test]
async fn cancel_when_nothing_runs_is_a_harmless_no_op() {
    let (state, vault_dir) = support::web::test_state(); // Ollama refused; no job ever starts
    seed(&vault_dir, IdeaState::InDiscussion);

    let (status, body) = post_form(state, "/idea/cancellable/cancel", "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("attack it"), "current transcript re-rendered");
    // A missing idea is still a 404.
}

#[tokio::test]
async fn cancel_of_a_missing_idea_is_404() {
    let (state, _vault_dir) = support::web::test_state();
    let (status, _) = post_form(state, "/idea/ghost/cancel", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn swarm_progress_note_is_surfaced_via_pending() {
    // Each model call completes after a short delay, and with K=1 the four angles run serially — so
    // the note advances ("swarm · attacking k/4: …") and a poll during the run catches it.
    let mock = spawn(
        &["llama3.2"],
        ChatScript::TokensAfterDelay {
            tokens: vec!["finding".into()],
            delay_ms: 120,
        },
    )
    .await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::InDiscussion);

    let (status, _) = post_form(state.clone(), "/idea/cancellable/swarm", "").await;
    assert_eq!(status, StatusCode::OK);
    // The live note reaches the indicator through /pending.
    let body =
        support::web::poll_until(state.clone(), "/idea/cancellable/pending", "swarm ·").await;
    assert!(
        body.contains("swarm ·"),
        "per-step progress note rendered in the indicator"
    );

    // And the run still converges to a persisted synthesis.
    support::web::poll_until(state, "/idea/cancellable/pending", "foil · swarm").await;
    let convo = store::read_conversation(&vault_dir, "cancellable").unwrap();
    assert_eq!(convo.matches("## assistant (swarm)").count(), 1);
    let _ = vault_dir;
}
