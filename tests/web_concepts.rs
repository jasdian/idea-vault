//! Web handler tests for R6 skill / R7 swarm (D18/D14): 200 `_turn.html` partials appended,
//! guards, and the persist rules (skill output appended by invoke; swarm persists only the
//! synthesis). Mock Ollama only.

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
                title: "Movable".into(),
                slug: "movable".into(),
                state,
                tags: vec![],
                created: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                updated: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
            },
            body: "The idea body.\n".into(),
        },
    )
    .unwrap();
    store::append_turn(vault, "movable", "user", "attack it").unwrap();
}

#[tokio::test]
async fn run_skill_returns_turn_partial_and_appends_it() {
    let mock = spawn(
        &["llama3.2"],
        ChatScript::Tokens(vec!["Ranked failure causes.".into()]),
    )
    .await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::InDiscussion);

    let (status, body) = post_form(state, "/idea/movable/skill/premortem", "").await;
    assert_eq!(status, StatusCode::OK);
    // The _turn.html partial with the labelled role and rendered content.
    assert!(
        body.contains("turn-assistant (skill: premortem)")
            || body.contains("assistant (skill: premortem)")
    );
    assert!(body.contains("Ranked failure causes."));

    // Persisted as a labelled assistant turn; the skill template reached the model.
    let convo = store::read_conversation(&vault_dir, "movable").unwrap();
    assert!(convo.contains("## assistant (skill: premortem)\nRanked failure causes."));
    assert!(mock.chat_bodies()[0].contains("failed badly 12 months"));
}

#[tokio::test]
async fn run_skill_guards_unknown_stored_and_missing() {
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec!["x".into()])).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::InDiscussion);

    let (status, _) = post_form(state.clone(), "/idea/movable/skill/not-a-skill", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = post_form(state.clone(), "/idea/ghost/skill/premortem", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(mock.chat_bodies().is_empty(), "no AI call for rejects");

    let mock2 = spawn(&["llama3.2"], ChatScript::Tokens(vec!["x".into()])).await;
    let (state2, vault_dir2) = test_state_with_ollama(&mock2.url, 1);
    seed(&vault_dir2, IdeaState::Stored);
    let (status, _) = post_form(state2, "/idea/movable/skill/premortem", "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn run_swarm_defaults_to_the_canonical_angles_and_persists_only_synthesis() {
    // Repeat script: every fan-out agent and the synthesizer answer the same way.
    let mock = spawn(
        &["llama3.2"],
        ChatScript::Tokens(vec!["converged finding".into()]),
    )
    .await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 2);
    seed(&vault_dir, IdeaState::InDiscussion);

    let (status, body) = post_form(state, "/idea/movable/swarm", "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("assistant (swarm)"));
    assert!(body.contains("converged finding"));

    // Canonical D14 set: 4 angles + 1 synthesizer = 5 model calls.
    assert_eq!(mock.chat_bodies().len(), 5);
    // Only the synthesis persisted, exactly one swarm turn.
    let convo = store::read_conversation(&vault_dir, "movable").unwrap();
    assert_eq!(convo.matches("## assistant (swarm)").count(), 1);
}

#[tokio::test]
async fn run_swarm_custom_angles_and_unknown_angle_400() {
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec!["out".into()])).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::InDiscussion);

    let (status, _) = post_form(state.clone(), "/idea/movable/swarm", "angles=premortem").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(mock.chat_bodies().len(), 2, "1 angle + 1 synthesizer");

    let (status, _) = post_form(state, "/idea/movable/swarm", "angles=nope").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn draft_ideas_refuse_moves_with_400_and_stay_untouched() {
    // D9 has no Draft skill/swarm edge — a Draft must never gain assistant turns while its
    // frontmatter still says draft (state is canonical).
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec!["x".into()])).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    store::write_idea(
        &vault_dir,
        &Idea {
            frontmatter: IdeaFrontmatter {
                title: "Drafty".into(),
                slug: "drafty".into(),
                state: IdeaState::Draft,
                tags: vec![],
                created: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                updated: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
            },
            body: "seed\n".into(),
        },
    )
    .unwrap();

    let (status, _) = post_form(state.clone(), "/idea/drafty/skill/premortem", "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = post_form(state, "/idea/drafty/swarm", "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(mock.chat_bodies().is_empty(), "no AI calls for a draft");
    assert_eq!(store::read_conversation(&vault_dir, "drafty").unwrap(), "");
}

#[tokio::test]
async fn oversized_angle_list_is_400_with_no_ai_calls() {
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec!["x".into()])).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::InDiscussion);

    let nine = std::iter::repeat_n("premortem", 9)
        .collect::<Vec<_>>()
        .join(",");
    let (status, _) = post_form(state, "/idea/movable/swarm", &format!("angles={nine}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(mock.chat_bodies().is_empty());
}

#[tokio::test]
async fn run_swarm_all_agents_failed_is_503_and_persists_nothing() {
    let mock = spawn(&["llama3.2"], ChatScript::EofAfter(vec![])).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::InDiscussion);
    let convo_before = store::read_conversation(&vault_dir, "movable").unwrap();

    let (status, _) = post_form(state, "/idea/movable/swarm", "").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        store::read_conversation(&vault_dir, "movable").unwrap(),
        convo_before
    );
}
