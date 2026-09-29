//! The build-plan artifact page (docs/adr/0029): the "Use it" box offers `PROMPT.md` and
//! `@plan.md` copy blocks derived at view time, escaped, and only for build-plan artifacts.

mod support;

use axum::http::StatusCode;
use chrono::{TimeZone, Utc};
use idea_vault::domain::{
    Artifact, ArtifactFrontmatter, ArtifactKind, Idea, IdeaFrontmatter, IdeaState,
};
use idea_vault::vault::store;
use support::web::{get, test_state};

const PLAN_BODY: &str = "# Build plan — Movable
_quick · m · 2026-09-28 21:40_
_gates: settled 1_

## Goal
Ship the parser.

## Settled
- S1: Parse in one pass. — you
  quote: \"parse in one pass\"

## Verify first
- none

## Open questions
- none

## Plan
- [ ] T1: Write the parser </pre><script>alert(1)</script>
  accept: `cargo test` → exit 0

## Kill criteria
- none

## Quarantined — do not build on these
- X1: The owner chose Rust
  reason: quote not in the discussion
";

fn seed(vault: &std::path::Path, kind: ArtifactKind, body: &str) -> String {
    let now = Utc.with_ymd_and_hms(2026, 9, 28, 21, 40, 0).unwrap();
    store::write_idea(
        vault,
        &Idea {
            frontmatter: IdeaFrontmatter {
                title: "Movable".into(),
                slug: "movable".into(),
                state: IdeaState::InDiscussion,
                tags: vec![],
                sources: vec![],
                created: now,
                updated: now,
            },
            body: "The idea body.\n".into(),
        },
    )
    .unwrap();
    let stem = "20260928-214000-build-plan".to_string();
    store::write_artifact(
        vault,
        "movable",
        &Artifact {
            frontmatter: ArtifactFrontmatter {
                slug: stem.clone(),
                title: "Build plan — Movable".into(),
                kind,
                lens: None,
                created: now,
                model: "m".into(),
            },
            body: body.into(),
        },
    )
    .unwrap();
    format!("/idea/movable/artifact/{stem}.md")
}

#[tokio::test]
async fn artifact_page_offers_prompt_and_attack_plan_copy_blocks() {
    let (state, vault) = test_state();
    let uri = seed(&vault, ArtifactKind::BuildPlan, PLAN_BODY);
    let (status, body) = get(state, &uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body.matches(r#"<pre class="copyable">"#).count(),
        2,
        "{body}"
    );
    assert!(body.contains("Use it") && body.contains("PROMPT.md") && body.contains("@plan.md"));
    assert!(body.contains("# Build: Ship the parser."), "{body}");
    assert!(body.contains("## PINNED — the owner said it"));
    assert!(body.contains("| [ ] | T | Task | Depends | score | model | accept |"));
    assert!(body.contains("## Do not build on"));
    assert!(body.contains("reason: quote not in the discussion"));
}

#[tokio::test]
async fn artifact_page_escapes_model_text_in_copy_blocks() {
    let (state, vault) = test_state();
    let uri = seed(&vault, ArtifactKind::BuildPlan, PLAN_BODY);
    let (_, body) = get(state, &uri).await;
    let start = body.find(r#"id="use-it""#).expect("use-it box");
    let block = &body[start..];
    assert!(!block.contains("<script>alert(1)"), "{block}");
    assert!(
        block.contains("&#60;/pre&#62;&#60;script&#62;alert(1)&#60;/script&#62;"),
        "{block}"
    );
}

#[tokio::test]
async fn artifact_page_for_other_kinds_has_no_copy_blocks() {
    let (state, vault) = test_state();
    let uri = seed(&vault, ArtifactKind::Finding, PLAN_BODY);
    let (status, body) = get(state, &uri).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !body.contains(r#"class="copyable""#) && !body.contains("Use it"),
        "{body}"
    );
}
