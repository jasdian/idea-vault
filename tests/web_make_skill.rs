//! Web tests for the make-skill button (docs/adr/0042, D42): R51 drafts a skill as a background
//! job that ends in a `skill_draft` artifact and a notice, never a turn; R52 saves the reviewed
//! draft into the skill book. Mock Ollama only.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; HTC-6 binds shipping code"
)]

mod support;

use axum::http::StatusCode;
use chrono::{TimeZone, Utc};
use idea_vault::ai::provenance::digest12;
use idea_vault::concepts::make_skill::{finalize, render_draft_body, Draft, EvidenceLine};
use idea_vault::domain::{
    Artifact, ArtifactFrontmatter, ArtifactKind, Idea, IdeaFrontmatter, IdeaState,
};
use idea_vault::vault::store;
use support::web::{get, poll_until, post_form, test_state, test_state_with_ollama};
use support::{spawn, ChatScript};

const SLUG: &str = "tutoring";

const CONVERSATION: &str = "## user\nA peer-tutoring marketplace for high-schoolers.\n\n\
## assistant (skill: premortem)\n1. **Regulators ban it** — child-safety law.\n\n\
## user\nNow assume a regulator hates it and wants it dead within a year.\n\n\
## assistant\nA hostile regulator would first classify the app as an employer.\n";

const GOOD_DRAFT: &str = "~~~skill\n---\nname: hostile-regulator\ndescription: \"Attack an idea as a regulator who wants it dead.\"\nstage: attack\nrole: critic\ncontract: ranked_list\n---\n\nAssume a regulator hates the idea below; list the rules they reach for, cheapest first.\n~~~\n\n## Evidence\n- \"assume a regulator hates it and wants it dead\"\n- \"regulators adore every tutoring startup\"\n";

fn seed(vault: &std::path::Path, state: IdeaState, conversation: &str) {
    store::write_idea(
        vault,
        &Idea {
            frontmatter: IdeaFrontmatter {
                title: "Tutoring".into(),
                slug: SLUG.into(),
                state,
                tags: vec![],
                sources: vec![],
                created: Utc.with_ymd_and_hms(2026, 9, 30, 10, 0, 0).unwrap(),
                updated: Utc.with_ymd_and_hms(2026, 9, 30, 10, 0, 0).unwrap(),
                extra: Default::default(),
            },
            body: "A peer-tutoring marketplace.\n".into(),
        },
    )
    .unwrap();
    if !conversation.is_empty() {
        store::append_conversation(vault, SLUG, conversation).unwrap();
    }
}

fn drafts(vault: &std::path::Path) -> Vec<idea_vault::domain::Artifact> {
    store::read_artifacts(vault, SLUG)
        .unwrap()
        .into_iter()
        .filter(|a| a.frontmatter.kind == ArtifactKind::SkillDraft)
        .collect()
}

#[tokio::test]
async fn r51_success_polls_to_the_notice_and_the_draft_artifact() {
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec![GOOD_DRAFT.into()])).await;
    let (state, vault) = test_state_with_ollama(&mock.url, 1);
    seed(&vault, IdeaState::InDiscussion, CONVERSATION);

    let (status, body) = post_form(state.clone(), &format!("/idea/{SLUG}/make-skill"), "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let done = poll_until(
        state.clone(),
        &format!("/idea/{SLUG}/pending"),
        "skill draft ready: hostile-regulator",
    )
    .await;
    assert!(
        done.contains("1 evidence quote"),
        "the ungrounded quote is warned about: {done}"
    );
    let d = drafts(&vault);
    assert_eq!(d.len(), 1);
    assert!(
        done.contains(&d[0].frontmatter.slug),
        "the artifacts panel lists the draft"
    );
    assert_eq!(
        store::read_conversation(&vault, SLUG).unwrap(),
        CONVERSATION,
        "no transcript turn"
    );
    assert_eq!(mock.chat_bodies().len(), 1);
}

#[tokio::test]
async fn r51_refuses_a_draft_idea_and_an_undistillable_one() {
    let (state, vault) = test_state();
    seed(&vault, IdeaState::Draft, "");
    let (status, _) = post_form(state.clone(), &format!("/idea/{SLUG}/make-skill"), "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (state, vault) = test_state();
    seed(
        &vault,
        IdeaState::InDiscussion,
        "## user\nhi\n\n## assistant (skill: premortem)\n1. x\n",
    );
    let (status, body) = post_form(state.clone(), &format!("/idea/{SLUG}/make-skill"), "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("nothing to distil yet"), "{body}");
    assert!(drafts(&vault).is_empty());
}

#[tokio::test]
async fn r51_unknown_idea_is_404() {
    let (state, _) = test_state();
    let (status, _) = post_form(state, "/idea/nope/make-skill", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn r51_busy_rerenders_the_transcript_without_a_second_job() {
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec![GOOD_DRAFT.into()])).await;
    let (state, vault) = test_state_with_ollama(&mock.url, 1);
    seed(&vault, IdeaState::InDiscussion, CONVERSATION);
    assert!(idea_vault::web::jobs::try_claim(&state.jobs, SLUG));

    let (status, body) = post_form(state.clone(), &format!("/idea/{SLUG}/make-skill"), "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("foil-pending"),
        "the in-flight indicator: {body}"
    );
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(mock.chat_bodies().is_empty(), "no second job ran");
    assert!(drafts(&vault).is_empty());
}

#[tokio::test]
async fn r51_button_is_on_the_capstones_row_and_disabled_while_busy() {
    // The discussion actions render only while the model is reachable (D20).
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec![GOOD_DRAFT.into()])).await;
    let (state, vault) = test_state_with_ollama(&mock.url, 1);
    seed(&vault, IdeaState::InDiscussion, CONVERSATION);
    let (_, page) = get(state.clone(), &format!("/idea/{SLUG}")).await;
    let form = format!("hx-post=\"/idea/{SLUG}/make-skill\"");
    assert!(page.contains(&form), "no make-skill button");
    let button = page.split(&form).nth(1).unwrap();
    let button = &button[..button.find("</form>").unwrap()];
    assert!(!button.contains(" disabled"), "{button}");

    assert!(idea_vault::web::jobs::try_claim(&state.jobs, SLUG));
    let (_, page) = get(state, &format!("/idea/{SLUG}")).await;
    let button = page.split(&form).nth(1).unwrap();
    let button = &button[..button.find("</form>").unwrap()];
    assert!(button.contains(" disabled"), "{button}");
}

const DRAFT_STEM: &str = "skill-draft-hostile-regulator";

/// The drafted file as the distiller would finalize it for this idea.
fn drafted_raw() -> String {
    let file = GOOD_DRAFT
        .split("~~~skill\n")
        .nth(1)
        .and_then(|rest| rest.split("\n~~~").next())
        .unwrap();
    finalize(file, SLUG).unwrap()
}

/// Seed an idea with a `skill_draft` artifact (as R51 leaves one) and return the draft's raw file.
fn seed_draft(vault: &std::path::Path, grounded: bool) -> String {
    seed(vault, IdeaState::InDiscussion, CONVERSATION);
    let raw = drafted_raw();
    let body = render_draft_body(&Draft {
        raw: raw.clone(),
        evidence: vec![EvidenceLine {
            quote: "assume a regulator hates it and wants it dead".into(),
            grounded,
            owner: grounded,
        }],
    });
    store::write_artifact(
        vault,
        SLUG,
        &Artifact {
            frontmatter: ArtifactFrontmatter {
                slug: DRAFT_STEM.into(),
                title: "Skill draft: hostile-regulator".into(),
                kind: ArtifactKind::SkillDraft,
                lens: Some("distill-skill".into()),
                created: Utc.with_ymd_and_hms(2026, 9, 30, 11, 0, 0).unwrap(),
                model: "llama3.2".into(),
                revises: None,
                version: None,
                answered: vec![],
                recipe: None,
            },
            body,
        },
    )
    .unwrap();
    raw
}

fn artifact_file(vault: &std::path::Path) -> String {
    std::fs::read_to_string(
        vault
            .join(SLUG)
            .join("artifacts")
            .join(format!("{DRAFT_STEM}.md")),
    )
    .unwrap()
}

fn save_uri() -> String {
    format!("/idea/{SLUG}/artifact/{DRAFT_STEM}.md/save-skill")
}

fn form(raw: &str, base_digest: Option<&str>) -> String {
    let mut fields = vec![("raw", raw)];
    if let Some(d) = base_digest {
        fields.push(("base_digest", d));
    }
    serde_urlencoded::to_string(fields).unwrap()
}

#[tokio::test]
async fn r52_add_writes_the_owner_skill_and_the_book_links_its_origin() {
    let (state, vault) = test_state();
    let raw = seed_draft(&vault, true);
    let before = artifact_file(&vault);

    let (status, page) = get(
        state.clone(),
        &format!("/idea/{SLUG}/artifact/{DRAFT_STEM}.md"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        page.contains("id=\"skill-draft\""),
        "the review panel: {page}"
    );
    assert!(page.contains("ADD hostile-regulator"), "{page}");
    assert!(page.contains("✓ owner"), "{page}");

    let (status, body) = post_form(state.clone(), &save_uri(), &form(&raw, None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("saved as"), "{body}");
    let saved = vault.join(".skills").join("hostile-regulator.md");
    assert_eq!(std::fs::read_to_string(&saved).unwrap(), raw);
    assert_eq!(
        artifact_file(&vault),
        before,
        "the draft artifact is never modified"
    );

    let (_, book) = get(state, "/skills").await;
    assert!(
        book.contains("hostile-regulator"),
        "reloaded without a restart"
    );
    assert!(book.contains(&format!("href=\"/idea/{SLUG}\"")), "{book}");
    assert!(book.contains(&format!("distilled from {SLUG}")), "{book}");
}

#[tokio::test]
async fn r52_update_shows_the_diff_and_needs_the_current_digest() {
    let (state, vault) = test_state();
    let raw = seed_draft(&vault, true);
    let current = raw.replace("cheapest first", "in any order");
    store::write_owner_skill(&vault.join(".skills"), "hostile-regulator", &current).unwrap();
    state.workflows.reload(&state.skills);

    let (_, page) = get(
        state.clone(),
        &format!("/idea/{SLUG}/artifact/{DRAFT_STEM}.md"),
    )
    .await;
    assert!(page.contains("UPDATE hostile-regulator"), "{page}");
    assert!(page.contains("diff--removed"), "{page}");
    let digest = digest12(current.as_bytes());
    assert!(page.contains(&digest), "the base digest rides the form");

    let (status, _) = post_form(state.clone(), &save_uri(), &form(&raw, None)).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "an unseen owner file is never overwritten"
    );
    let (status, _) = post_form(
        state.clone(),
        &save_uri(),
        &form(&raw, Some("000000000000")),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "a stale base");
    let skills_file = vault.join(".skills").join("hostile-regulator.md");
    assert_eq!(std::fs::read_to_string(&skills_file).unwrap(), current);

    let (status, body) = post_form(state, &save_uri(), &form(&raw, Some(&digest))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(std::fs::read_to_string(&skills_file).unwrap(), raw);
}

#[tokio::test]
async fn r52_refuses_builtin_internal_and_invalid_text_with_the_panel() {
    let (state, vault) = test_state();
    let raw = seed_draft(&vault, true);
    for (edited, needle) in [
        (
            raw.replace("name: hostile-regulator", "name: premortem"),
            "built-in",
        ),
        (
            raw.replace("name: hostile-regulator", "name: distill-skill"),
            "engine-only",
        ),
        (raw.replace("{context}", ""), "{context}"),
    ] {
        let (status, body) = post_form(state.clone(), &save_uri(), &form(&edited, None)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{edited}");
        assert!(
            body.contains("id=\"skill-draft\""),
            "the panel comes back: {body}"
        );
        assert!(body.contains(needle), "{needle}: {body}");
    }
    assert!(!vault.join(".skills").exists(), "nothing written");
    assert!(!vault.join(".skills").join("premortem.md").exists());
}

#[tokio::test]
async fn r52_ungrounded_evidence_warns_but_does_not_block() {
    let (state, vault) = test_state();
    let raw = seed_draft(&vault, false);
    let (_, page) = get(
        state.clone(),
        &format!("/idea/{SLUG}/artifact/{DRAFT_STEM}.md"),
    )
    .await;
    assert!(
        page.contains("skilldraft__warn"),
        "D3 warning shown: {page}"
    );
    let (status, _) = post_form(state, &save_uri(), &form(&raw, None)).await;
    assert_eq!(status, StatusCode::OK, "D3: grounding never blocks a save");
    assert!(vault.join(".skills").join("hostile-regulator.md").exists());
}

#[tokio::test]
async fn r52_not_a_skill_draft_is_404() {
    let (state, vault) = test_state();
    let raw = seed_draft(&vault, true);
    let (status, _) = post_form(
        state.clone(),
        &format!("/idea/{SLUG}/artifact/absent.md/save-skill"),
        &form(&raw, None),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let mut other = store::read_artifact(&vault, SLUG, DRAFT_STEM).unwrap();
    other.frontmatter.slug = "a-finding".into();
    other.frontmatter.kind = ArtifactKind::Finding;
    store::write_artifact(&vault, SLUG, &other).unwrap();
    let (status, _) = post_form(
        state.clone(),
        &format!("/idea/{SLUG}/artifact/a-finding.md/save-skill"),
        &form(&raw, None),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = post_form(
        state,
        "/idea/nope/artifact/x.md/save-skill",
        &form(&raw, None),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(!vault.join(".skills").exists());
}
