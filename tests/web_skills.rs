//! The skill book (`GET /skills`, `POST /skills/reload`, ADR-0022) and owner-authored skills in
//! `vault/.skills/`: listing, broken-file surfacing, live reload into a runnable move, and the
//! swarm route's capstone guard. Mock Ollama only.

mod support;

use axum::http::StatusCode;
use chrono::{TimeZone, Utc};
use idea_vault::domain::{Idea, IdeaFrontmatter, IdeaState};
use idea_vault::vault::store;
use support::web::{get, post_form, test_state, test_state_with_ollama};
use support::{spawn, ChatScript};

fn seed(vault: &std::path::Path) {
    store::write_idea(
        vault,
        &Idea {
            frontmatter: IdeaFrontmatter {
                title: "Skilled".into(),
                slug: "skilled".into(),
                state: IdeaState::InDiscussion,
                tags: vec![],
                sources: vec![],
                created: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                updated: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
            },
            body: "The idea body.\n".into(),
        },
    )
    .unwrap();
    store::append_turn(vault, "skilled", "user", "go").unwrap();
}

const OWNER_SKILL: &str = "---\n\
name: pre-mortem-lite\n\
description: A quick owner-authored failure pass.\n\
stage: attack\n\
use_when: Five minutes before a meeting.\n\
---\n\
\n\
OWNER-PROMPT-MARKER: list three ways this dies.\n\
{context}\n";

#[tokio::test]
async fn skill_book_lists_builtins_by_stage_with_guidance() {
    let (state, _) = test_state();
    let (status, body) = get(state, "/skills").await;
    assert_eq!(status, StatusCode::OK);
    for heading in ["steelman", "attack", "consequence", "converge", "capstone"] {
        assert!(body.contains(heading), "missing stage {heading}");
    }
    assert!(body.contains("premortem"));
    assert!(body.contains("use when"));
    assert!(
        body.contains("extract-key-decisions"),
        "hidden lenses stay documented"
    );
}

#[tokio::test]
async fn a_broken_owner_file_is_listed_and_never_breaks_the_page() {
    let (state, vault_dir) = test_state();
    let dir = vault_dir.join(".skills");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("oops.md"), "no frontmatter here").unwrap();

    let (status, body) = post_form(state.clone(), "/skills/reload", "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("oops.md"), "issue not surfaced:\n{body}");
    let (status, page) = get(state, "/skills").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("oops.md"));
    assert!(page.contains("premortem"), "built-ins still listed");
}

#[tokio::test]
async fn reload_makes_an_owner_skill_a_runnable_move() {
    let mock = spawn(
        &["llama3.2"],
        ChatScript::Tokens(vec!["1. Owner-shaped failure.".into()]),
    )
    .await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir);

    let (status, _) = post_form(state.clone(), "/idea/skilled/skill/pre-mortem-lite", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown until reloaded");

    let dir = vault_dir.join(".skills");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("pre-mortem-lite.md"), OWNER_SKILL).unwrap();
    let (_, book) = post_form(state.clone(), "/skills/reload", "").await;
    assert!(book.contains("pre-mortem-lite") && book.contains("vault"));

    let (_, page) = get(state.clone(), "/idea/skilled").await;
    assert!(
        page.contains("/idea/skilled/skill/pre-mortem-lite"),
        "owner move not offered as a chip"
    );
    assert!(page.contains("use when: Five minutes before a meeting."));

    let (status, _) = post_form(state.clone(), "/idea/skilled/skill/pre-mortem-lite", "").await;
    assert_eq!(status, StatusCode::OK);
    support::web::poll_until(state, "/idea/skilled/pending", "Owner-shaped failure").await;
    assert!(mock.chat_bodies()[0].contains("OWNER-PROMPT-MARKER"));
    let convo = store::read_conversation(&vault_dir, "skilled").unwrap();
    assert!(convo.contains("## assistant (skill: pre-mortem-lite)"));
}

#[tokio::test]
async fn swarm_rejects_a_capstone_as_an_angle() {
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec!["x".into()])).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir);
    let (status, body) = post_form(state, "/idea/skilled/swarm", "angles=build-prompt").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("capstone"));
    assert!(mock.chat_bodies().is_empty());
}

#[tokio::test]
async fn swarm_rejects_a_converge_move_as_an_angle() {
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec!["x".into()])).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir);
    let (status, body) = post_form(state, "/idea/skilled/swarm", "angles=converge").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("converge move"));
    assert!(mock.chat_bodies().is_empty());
}
