//! The skill book (`GET /skills`, `POST /skills/reload`, ADR-0022) and owner-authored skills in
//! `vault/.skills/`: listing, broken-file surfacing, live reload into a runnable move, and the
//! swarm route's capstone guard. Mock Ollama only.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; HTC-6 binds shipping code"
)]

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
async fn override_digest_shown() {
    let (state, vault_dir) = test_state();
    let builtin = state
        .skills
        .snapshot()
        .get("premortem")
        .unwrap()
        .digest
        .clone();
    let (_, page) = get(state.clone(), "/skills").await;
    assert!(
        page.contains(&format!("@{builtin}")),
        "built-in digest shown"
    );

    let dir = vault_dir.join(".skills");
    std::fs::create_dir_all(&dir).unwrap();
    let edited = format!(
        "{}\nName the first domino.\n",
        include_str!("../src/concepts/skills/premortem.md").trim_end()
    );
    std::fs::write(dir.join("premortem.md"), edited).unwrap();
    let (status, book) = post_form(state.clone(), "/skills/reload", "").await;
    assert_eq!(status, StatusCode::OK);
    let over = state
        .skills
        .snapshot()
        .get("premortem")
        .unwrap()
        .digest
        .clone();
    assert_ne!(over, builtin);
    assert!(book.contains(&format!("@{over}")), "the override's digest");
    assert!(
        !book.contains(&format!("@{builtin}")),
        "the built-in's is gone"
    );
    let row = &book[book.find(&format!("@{over}")).unwrap().saturating_sub(400)..];
    assert!(row.contains("vault override"), "the override is marked");
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

/// A valid owner workflow over a built-in skill: ceiling 2, one wave.
const OWNER_WORKFLOW: &str = "---
name: quick-check
description: An owner-authored one-angle check.
use_when: A two-minute sanity pass.
stages:
  - kind: fan_out
    steps:
      - {role: critic, skill: premortem}
  - kind: synthesize
---

OWNER-WORKFLOW-BODY: one premortem, then converge.
";

/// An owner workflow that needs the owner skill `pre-mortem-lite`.
const DEPENDENT_WORKFLOW: &str = "---
name: lite-pass
description: Runs the owner's lite premortem.
stages:
  - kind: fan_out
    steps:
      - {role: critic, skill: pre-mortem-lite}
  - kind: synthesize
---
";

fn write_workflow(state: &idea_vault::app::AppState, file: &str, raw: &str) {
    let dir = state.workflows.dir().to_path_buf();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(file), raw).unwrap();
}

/// The workflow group of a rendered book (page or reload response).
fn workflow_group(body: &str) -> &str {
    body.split("id=\"workflows\"")
        .nth(1)
        .expect("the book has a workflows group")
}

#[tokio::test]
async fn book_lists_workflows_with_ceiling_and_issue_banner() {
    let (state, _) = test_state();
    let (status, body) = get(state.clone(), "/skills").await;
    assert_eq!(status, StatusCode::OK);
    let book = workflow_group(&body);
    for name in [
        "interrogate",
        "steelman-then-attack",
        "design-panel",
        "exhaust",
        "ready-to-build",
    ] {
        assert!(
            book.contains(&format!("href=\"/skills/workflow/{name}\"")),
            "{name} not on the book"
        );
    }
    // The exact ceilings (ADR-0034), with waves at the harness's K=1. exhaust's loop is
    // min(12 calls, 3 rounds × 3 steps) = 9, so its run is 13, inside the accepted ≤16.
    assert!(book.contains("up to 12 model calls · widest stage 3 → 3 waves at K=1"));
    assert!(book.contains("up to 13 model calls · widest stage 3 → 3 waves at K=1"));
    assert!(book.contains("grounds in attached sources"));
    assert!(!book.contains("workflow file"), "no banner while clean");

    // An owner file naming an unknown kind is an issue on the book, never a broken page.
    write_workflow(
        &state,
        "typo.md",
        "---\nname: typo\ndescription: d\nstages:\n  - kind: fan_outt\n---\n",
    );
    let (status, body) = post_form(state.clone(), "/skills/reload", "").await;
    assert_eq!(status, StatusCode::OK);
    let book = workflow_group(&body);
    assert!(book.contains("1 workflow file skipped"), "{book}");
    assert!(book.contains("<code>typo.md</code>"), "{book}");
    assert!(
        book.contains("stages[0]"),
        "the issue names the stage: {book}"
    );
    assert!(book.contains("design-panel"), "built-ins still listed");
    let (_, page) = get(state, "/skills").await;
    assert!(workflow_group(&page).contains("<code>typo.md</code>"));
}

#[tokio::test]
async fn reload_revalidates_workflows_same_response() {
    let (state, vault_dir) = test_state();
    let skills = vault_dir.join(".skills");
    std::fs::create_dir_all(&skills).unwrap();
    write_workflow(&state, "lite-pass.md", DEPENDENT_WORKFLOW);

    // Skill and dependent workflow land in the same reload: skills are re-read first, then the
    // workflows are validated against that fresh snapshot (ADR-0035).
    std::fs::write(skills.join("pre-mortem-lite.md"), OWNER_SKILL).unwrap();
    let (status, body) = post_form(state.clone(), "/skills/reload", "").await;
    assert_eq!(status, StatusCode::OK);
    let book = workflow_group(&body);
    assert!(
        book.contains("href=\"/skills/workflow/lite-pass\""),
        "{book}"
    );
    assert!(!book.contains("workflow file"), "{book}");

    // Removing the skill invalidates the workflow in the very same reload response.
    std::fs::remove_file(skills.join("pre-mortem-lite.md")).unwrap();
    let (status, body) = post_form(state.clone(), "/skills/reload", "").await;
    assert_eq!(status, StatusCode::OK);
    let book = workflow_group(&body);
    assert!(!book.contains("href=\"/skills/workflow/lite-pass\""));
    assert!(book.contains("<code>lite-pass.md</code>"), "{book}");
    assert!(
        book.contains("pre-mortem-lite"),
        "the issue names the skill"
    );
    let (status, _) = get(state, "/skills/workflow/lite-pass").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn r49_renders_builtin_and_vault_workflow_and_404s_unknown() {
    let (state, _) = test_state();

    let (status, page) = get(state.clone(), "/skills/workflow/design-panel").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("<h1 class=\"title\">design-panel</h1>"));
    assert!(page.contains("built-in"));
    assert!(page.contains("up to 12 model calls"));
    for kind in ["ground", "panel", "audit", "synthesize"] {
        assert!(
            page.contains(&format!("<span class=\"book__kind\">{kind}</span>")),
            "{kind}"
        );
    }
    // The panel's rubric, the rendered body and the file as loaded.
    assert!(page.contains("<code>evidence</code>") && page.contains("×2"));
    assert!(page.contains("The design panel (ADR-0034)"));
    assert!(page.contains("kind: panel"));
    assert!(page.contains("skipped at no cost"), "no-sources note");

    write_workflow(&state, "quick-check.md", OWNER_WORKFLOW);
    let (status, _) = post_form(state.clone(), "/skills/reload", "").await;
    assert_eq!(status, StatusCode::OK);
    let (status, page) = get(state.clone(), "/skills/workflow/quick-check").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains(">vault</span>"), "{page}");
    assert!(page.contains("OWNER-WORKFLOW-BODY"));
    assert!(page.contains("critic·premortem"));
    assert!(page.contains("up to 2 model calls · widest stage 1 → 1 wave at K=1"));
    assert!(!page.contains("skipped at no cost"), "no Ground, no note");

    let (status, _) = get(state.clone(), "/skills/workflow/no-such-flow").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get(state, "/skills/workflow/..%2Fetc").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
