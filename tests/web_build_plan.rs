//! The build-plan artifact page (docs/adr/0030): the "Use it" box offers `PROMPT.md` and
//! `plan.md` copy blocks derived at view time, escaped, and only for build-plan artifacts.

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
                revises: None,
                version: None,
                answered: Vec::new(),
                recipe: None,
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
    assert!(body.contains("Use it") && body.contains("PROMPT.md"));
    assert!(
        body.contains(r#"<h3 class="useit__label">plan.md</h3>"#)
            && !body.contains(concat!("@", "plan")),
        "{body}"
    );
    assert!(body.contains("# Build: Ship the parser."), "{body}");
    assert!(body.contains("## PINNED — the owner said it"));
    assert!(body.contains("| [ ] | T | Task | Depends | wave | score | model | touches | accept |"));
    assert!(body.contains("## Do not build on"));
    assert!(body.contains("reason: quote not in the discussion"));
}

fn copy_blocks(body: &str) -> Vec<&str> {
    body.split(r#"<pre class="copyable">"#)
        .skip(1)
        .map(|b| &b[..b.find("</pre>").expect("a closed copy block")])
        .collect()
}

#[tokio::test]
async fn artifact_page_prompt_block_carries_the_run_protocol_and_trust_line() {
    let (state, vault) = test_state();
    let uri = seed(&vault, ArtifactKind::BuildPlan, PLAN_BODY);
    let (_, body) = get(state, &uri).await;
    let blocks = copy_blocks(&body);
    assert_eq!(blocks.len(), 2, "{body}");
    assert!(
        blocks[0].contains("\n## How to run this\n"),
        "{}",
        blocks[0]
    );
    assert!(
        blocks[0].contains("_trust: quick · audit tally not recorded · generated 2026-09-28 21:40 by m · sources not recorded"),
        "{}",
        blocks[0]
    );
}

/// ADR-0041: the findings protocol reaches the owner's copy of `PROMPT.md` before the pinned
/// items it governs, and `plan.md` still points at the run protocol and names the Findings rule.
#[tokio::test]
async fn prompt_md_carries_findings_protocol_end_to_end() {
    let (state, vault) = test_state();
    let uri = seed(&vault, ArtifactKind::BuildPlan, PLAN_BODY);
    let (status, body) = get(state, &uri).await;
    assert_eq!(status, StatusCode::OK);
    let blocks = copy_blocks(&body);
    assert_eq!(blocks.len(), 2, "{body}");
    let prompt = blocks[0];
    let ask = prompt.find("(c) ask-user — ").expect("the ask-user clause");
    let pinned = prompt.find("## PINNED").expect("the PINNED section");
    assert!(ask < pinned, "{prompt}");
    assert!(
        blocks[1].contains("Rules: PROMPT.md (How to run this, Findings, PINNED, Fence)"),
        "{}",
        blocks[1]
    );
}

const PREMISE_BODY: &str ="# Build plan — Movable
_quick · unaudited · m · 2026-09-28 21:40 · 0 capstone turn(s) excluded from evidence · consulted: none · sources: none · audit: none_
_gates: premises 1 · tasks 1_

## Goal
Ship the parser.

## Settled
- none

## Verify first
- P1: The parser has a main entry
  check: `grep -c <main> src/a.rs | grep -q 1` → exit 0

## Open questions
- none

## Plan
- [ ] T1: Write the parser
  depends: P1
  touches: src/a.rs
  accept: `cargo test` → exit 0
  model: sonnet
  score: 00000
  wave: 1

## Kill criteria
- none
";

#[tokio::test]
async fn artifact_page_attack_plan_block_opens_with_the_escaped_bootstrap_row() {
    let (state, vault) = test_state();
    let uri = seed(&vault, ArtifactKind::BuildPlan, PREMISE_BODY);
    let (status, body) = get(state, &uri).await;
    assert_eq!(status, StatusCode::OK);
    let blocks = copy_blocks(&body);
    assert_eq!(blocks.len(), 2, "{body}");
    let t0 = "| [ ] | T0 | Run the bootstrap checks P1 (read-only, no commit); mark T0 [x] once every check has run, log each failed P# and mark [?] every task whose premises list it | — | 0 | 00000 | haiku | none (read-only) | P1: `grep -c &#60;main&#62; src/a.rs \\| grep -q 1` → exit 0 |";
    assert!(blocks[1].contains(t0), "{}", blocks[1]);
    assert!(
        blocks[1].contains(
            "| [ ] | T1 | Write the parser (premises: P1) | T0 | 1 | 00000 | sonnet | src/a.rs |"
        ),
        "{}",
        blocks[1]
    );
    assert!(!blocks[1].contains("<main>"), "{}", blocks[1]);
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

fn copy_script(body: &str) -> &str {
    let start = body
        .find("function addCopyButtons()")
        .expect("the copy script");
    let rest = &body[start..];
    let end = rest
        .find("document.addEventListener(\"DOMContentLoaded\", addCopyButtons)")
        .expect("the end of the copy script");
    &rest[..end]
}

#[tokio::test]
async fn copy_label_handler_reads_a_button_free_clone() {
    let (state, vault) = test_state();
    let uri = seed(&vault, ArtifactKind::BuildPlan, PLAN_BODY);
    let (status, body) = get(state, &uri).await;
    assert_eq!(status, StatusCode::OK);
    let script = copy_script(&body);
    let click = script
        .find(r#"addEventListener("click""#)
        .expect("a click handler");
    let handler = &script[click..];
    let handler = &handler[..handler.find(".then(").expect("the clipboard write")];
    let pos = |needle: &str| {
        handler
            .find(needle)
            .unwrap_or_else(|| panic!("{needle} missing from {handler}"))
    };
    let cloned = pos("pre.cloneNode(true)");
    let stripped = pos(r#"clone.querySelectorAll(".copy-btn")"#);
    let removed = pos(".remove()");
    let written = pos("writeText(clone.textContent");
    assert!(
        cloned < stripped && stripped < removed && removed < written,
        "clone, strip, then write: {handler}"
    );
    assert!(
        !script.contains("innerText"),
        "no copy path may read the live pre: {script}"
    );
    assert!(
        script.matches("writeText(").count() == 1,
        "one clipboard write, from the clone: {script}"
    );
}

#[tokio::test]
async fn copy_label_selector_covers_transcript_and_copyable_blocks() {
    let (state, vault) = test_state();
    let uri = seed(&vault, ArtifactKind::BuildPlan, PLAN_BODY);
    let (_, body) = get(state, &uri).await;
    assert!(
        copy_script(&body).contains(r#"querySelectorAll(".turn__body pre, pre.copyable")"#),
        "the selector must still reach pre.copyable"
    );
}
