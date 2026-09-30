//! Web tests for the make-skill button (docs/adr/0042, D42): R51 drafts a skill as a background
//! job that ends in a `skill_draft` artifact and a notice, never a turn; R52 saves the reviewed
//! draft into the skill book. Mock Ollama only.

mod support;

use axum::http::StatusCode;
use chrono::{TimeZone, Utc};
use idea_vault::domain::{ArtifactKind, Idea, IdeaFrontmatter, IdeaState};
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
