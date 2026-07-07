//! Web-surface tests for auto-compact (docs/adr/0012): the honest effective-bytes meter + summary
//! disclosure, the manual `/compact` route, and the job-isolation guarantee that a failed phase-0
//! compaction never fails the chat reply. Mock Ollama only.

mod support;

use axum::http::StatusCode;
use chrono::{TimeZone, Utc};
use idea_vault::domain::{Compacted, CompactedFrontmatter, Idea, IdeaFrontmatter, IdeaState};
use idea_vault::vault::store;
use support::web::{get, poll_until, post_form, test_state, test_state_with_ollama};
use support::{spawn_sequence, ChatScript};

fn seed(vault: &std::path::Path, slug: &str, state: IdeaState) {
    store::write_idea(
        vault,
        &Idea {
            frontmatter: IdeaFrontmatter {
                title: "Metered".into(),
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

fn prefix_bytes(turns: &[String], k: usize) -> usize {
    turns.iter().take(k).map(|t| t.trim_end().len() + 1).sum()
}

#[tokio::test]
async fn meter_shows_effective_bytes_and_compacted_through_after_a_fold() {
    let (state, vault) = test_state();
    seed(&vault, "i", IdeaState::InDiscussion);
    // A big transcript: raw size well over budget so the pre-fold meter would pin at the cap.
    for i in 0..8 {
        store::append_turn(
            &vault,
            "i",
            "user",
            &format!("turn {i} {}", "x".repeat(1500)),
        )
        .unwrap();
    }
    let turns = store::split_turns(&store::read_conversation(&vault, "i").unwrap());
    let k = 5;
    store::write_compacted(
        &vault,
        "i",
        &Compacted {
            frontmatter: CompactedFrontmatter {
                compacted_through: k,
                covered_bytes: prefix_bytes(&turns, k),
                turn_count_at_compaction: turns.len(),
                model: "test".into(),
                updated: Utc::now(),
            },
            summary: "## Decisions\n- a compact rolling summary".into(),
        },
    )
    .unwrap();

    let (status, body) = get(state, "/idea/i").await;
    assert_eq!(status, StatusCode::OK);
    // Honest meter: still reports all turns, but the effective size + the compaction note.
    assert!(body.contains(&format!("{} turns", turns.len())));
    assert!(
        body.contains(&format!("compacted through turn {k}")),
        "meter names the fold boundary"
    );
    // The derived summary is surfaced (not hidden) via the disclosure.
    assert!(body.contains("Summary of earlier turns (used for AI context)"));
    assert!(body.contains("a compact rolling summary"));
}

#[tokio::test]
async fn a_stale_compacted_md_is_not_shown_as_active_context() {
    let (state, vault) = test_state();
    seed(&vault, "i", IdeaState::InDiscussion);
    for i in 0..4 {
        store::append_turn(&vault, "i", "user", &format!("turn {i}")).unwrap();
    }
    // Deliberately wrong fingerprint → not applied → no disclosure, no "compacted through".
    store::write_compacted(
        &vault,
        "i",
        &Compacted {
            frontmatter: CompactedFrontmatter {
                compacted_through: 2,
                covered_bytes: 999_999,
                turn_count_at_compaction: 4,
                model: "test".into(),
                updated: Utc::now(),
            },
            summary: "STALE".into(),
        },
    )
    .unwrap();
    let (_, body) = get(state, "/idea/i").await;
    assert!(!body.contains("compacted through turn"));
    assert!(!body.contains("Summary of earlier turns"));
}

#[tokio::test]
async fn manual_compact_folds_and_is_refused_on_a_stored_idea() {
    // Over-threshold transcript so the manual fold actually advances.
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![ChatScript::Tokens(vec![
            "## Decisions\n- folded by hand".into()
        ])],
    )
    .await;
    let (state, vault) = test_state_with_ollama(&mock.url, 2);
    seed(&vault, "i", IdeaState::InDiscussion);
    for i in 0..10 {
        store::append_turn(&vault, "i", "user", &format!("m{i} {}", "x".repeat(1000))).unwrap();
    }

    let (status, _) = post_form(state.clone(), "/idea/i/compact", "").await;
    assert_eq!(status, StatusCode::OK);
    // The fold lands in the background — poll the transcript until the meter shows a boundary.
    poll_until(state, "/idea/i/pending", "compacted through turn").await;
    let compacted = store::read_compacted(&vault, "i").unwrap().unwrap();
    assert!(compacted.frontmatter.compacted_through > 0);
    assert_eq!(compacted.summary, "## Decisions\n- folded by hand");

    // Stored → refused.
    let (state2, vault2) = test_state();
    seed(&vault2, "s", IdeaState::Stored);
    let (status, _) = post_form(state2, "/idea/s/compact", "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_failed_phase0_compaction_never_fails_the_reply() {
    // Chat on an over-threshold idea: phase-0 compaction FAILS (call 1 EOFs), but the reply
    // (call 2) still succeeds and the job marks done — no spurious "could not respond".
    let mock = spawn_sequence(
        &["llama3.2"],
        vec![
            ChatScript::EofAfter(vec!["broken".into()]), // phase 0: compaction fails
            ChatScript::Tokens(vec!["A good reply.".into()]), // phase 1: the reply succeeds
        ],
    )
    .await;
    let (state, vault) = test_state_with_ollama(&mock.url, 2);
    seed(&vault, "i", IdeaState::InDiscussion);
    // Push effective size over the 0.80 threshold so phase-0 compaction actually runs.
    for i in 0..12 {
        store::append_turn(&vault, "i", "user", &format!("m{i} {}", "x".repeat(1200))).unwrap();
    }

    let (status, _) = post_form(state.clone(), "/idea/i/chat", "message=push").await;
    assert_eq!(status, StatusCode::OK);
    let final_body = poll_until(state, "/idea/i/pending", "A good reply.").await;
    assert!(
        !final_body.contains("could not respond"),
        "a compaction failure must not surface as a failed reply"
    );
    let convo = store::read_conversation(&vault, "i").unwrap();
    assert!(convo.contains("## assistant\nA good reply."));
}
