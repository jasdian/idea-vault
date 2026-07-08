//! Web handler tests for R23 rename (docs/09-web-ui.md, docs/04-state-machine.md "Rename is
//! orthogonal to D9"): retitling in place through the real router — truth on disk (title +
//! `updated` bumped, slug/state/body untouched), the reindex that keeps list/search rows current,
//! input validation, and that it works from every state including `Stored`.

mod support;

use axum::http::StatusCode;
use chrono::{TimeZone, Utc};
use idea_vault::domain::{Idea, IdeaFrontmatter, IdeaState};
use idea_vault::vault::store;
use support::web::{get, post_form, test_state};

/// Seed an idea directly on disk (no AI calls needed — rename never touches the model), with a
/// distinguishing body sentence so full-text search can find it by content instead of by title.
fn seed(vault: &std::path::Path, slug: &str, title: &str, state: IdeaState) {
    store::write_idea(
        vault,
        &Idea {
            frontmatter: IdeaFrontmatter {
                title: title.into(),
                slug: slug.into(),
                state,
                tags: vec![],
                created: Utc.with_ymd_and_hms(2026, 7, 1, 9, 0, 0).unwrap(),
                updated: Utc.with_ymd_and_hms(2026, 7, 1, 9, 0, 0).unwrap(),
            },
            body: "A statement about wombats and their burrows.\n".into(),
        },
    )
    .unwrap();
}

#[tokio::test]
async fn rename_updates_title_and_stamp_but_leaves_state_slug_and_body_untouched() {
    let (state, vault_dir) = test_state();
    seed(
        &vault_dir,
        "wombat-idea",
        "Original Title",
        IdeaState::InDiscussion,
    );

    let (status, body) = post_form(
        state,
        "/idea/wombat-idea/rename",
        "title=A%20Much%20Better%20Title",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // The response is the re-rendered title block, carrying the new title straight away.
    assert!(body.contains("A Much Better Title"), "{body}");
    assert!(body.contains("id=\"idea-title\""));
    // The rename hint names the (unchanged) slug so the owner sees links keep working.
    assert!(body.contains("wombat-idea"));

    let idea = store::read_idea(&vault_dir, "wombat-idea").unwrap();
    assert_eq!(idea.frontmatter.title, "A Much Better Title");
    assert_eq!(idea.frontmatter.slug, "wombat-idea", "slug is immutable");
    assert_eq!(
        idea.frontmatter.state,
        IdeaState::InDiscussion,
        "state untouched"
    );
    assert_eq!(
        idea.body, "A statement about wombats and their burrows.\n",
        "body untouched"
    );
    assert!(
        idea.frontmatter.updated > Utc.with_ymd_and_hms(2026, 7, 1, 9, 0, 0).unwrap(),
        "updated must be bumped"
    );
}

#[tokio::test]
async fn rename_works_on_a_stored_idea_without_touching_its_state() {
    let (state, vault_dir) = test_state();
    seed(
        &vault_dir,
        "wombat-idea",
        "Original Title",
        IdeaState::Stored,
    );

    let (status, _) = post_form(
        state,
        "/idea/wombat-idea/rename",
        "title=Stored%20But%20Renamed",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "rename is legal from every D9 state"
    );

    let idea = store::read_idea(&vault_dir, "wombat-idea").unwrap();
    assert_eq!(idea.frontmatter.title, "Stored But Renamed");
    assert_eq!(
        idea.frontmatter.state,
        IdeaState::Stored,
        "storing is not undone"
    );
}

#[tokio::test]
async fn empty_or_whitespace_title_is_rejected_with_400_and_leaves_the_original_title() {
    let (state, vault_dir) = test_state();
    seed(
        &vault_dir,
        "wombat-idea",
        "Original Title",
        IdeaState::Draft,
    );

    let (s1, _) = post_form(state.clone(), "/idea/wombat-idea/rename", "title=").await;
    assert_eq!(s1, StatusCode::BAD_REQUEST);
    let (s2, _) = post_form(state.clone(), "/idea/wombat-idea/rename", "title=%20%20%20").await;
    assert_eq!(s2, StatusCode::BAD_REQUEST);

    let idea = store::read_idea(&vault_dir, "wombat-idea").unwrap();
    assert_eq!(
        idea.frontmatter.title, "Original Title",
        "rejected renames must not land"
    );
}

#[tokio::test]
async fn renaming_an_unknown_slug_is_404() {
    let (state, _vault_dir) = test_state();
    let (status, _) = post_form(state, "/idea/does-not-exist/rename", "title=New%20Name").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn renamed_title_refreshes_in_the_list_and_in_search_results() {
    let (state, vault_dir) = test_state();
    seed(
        &vault_dir,
        "wombat-idea",
        "Original Title",
        IdeaState::InDiscussion,
    );

    let (status, _) = post_form(
        state.clone(),
        "/idea/wombat-idea/rename",
        "title=Wombat%20Renamed",
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The list page (R1) reads titles off the index — the rename's own reindex must have already
    // refreshed the row, not just the frontmatter file.
    let (_, list_body) = get(state.clone(), "/").await;
    assert!(list_body.contains("Wombat Renamed"), "{list_body}");
    assert!(!list_body.contains("Original Title"), "{list_body}");

    // Full-text search matches on body content ("wombats"), not on title text — but the result
    // row's *displayed* title must be the current one, proving the reindex updated the `ideas`
    // row the search join reads title from, not just a stale FTS snapshot.
    let (_, search_body) = get(state, "/search?q=wombats").await;
    assert!(search_body.contains("Wombat Renamed"), "{search_body}");
    assert!(!search_body.contains("Original Title"), "{search_body}");
}

#[tokio::test]
async fn rename_refuses_while_a_job_is_running_and_recovers_after() {
    // The critical race the review confirmed: rename is a whole-file read-modify-write, so it
    // must be serialized against the per-idea job slot — a store job's stale snapshot would
    // otherwise clobber the new title (or the rename revert a completed Stored transition).
    let (state, vault_dir) = support::web::test_state();
    seed(
        &vault_dir,
        "busy-idea",
        "Original Title",
        IdeaState::InDiscussion,
    );

    assert!(idea_vault::web::jobs::try_claim(&state.jobs, "busy-idea"));
    let (status, body) =
        post_form(state.clone(), "/idea/busy-idea/rename", "title=New+Title").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("run is in progress"), "{body}");
    assert_eq!(
        store::read_idea(&vault_dir, "busy-idea")
            .unwrap()
            .frontmatter
            .title,
        "Original Title",
        "busy rename must not touch truth"
    );

    // Slot freed -> rename works, and the slot it claimed for itself is released again.
    idea_vault::web::jobs::mark_done(&state.jobs, "busy-idea");
    let (status, _) = post_form(state.clone(), "/idea/busy-idea/rename", "title=New+Title").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        idea_vault::web::jobs::try_claim(&state.jobs, "busy-idea"),
        "slot released after rename"
    );
}

#[tokio::test]
async fn rename_rejects_control_characters() {
    let (state, vault_dir) = support::web::test_state();
    seed(
        &vault_dir,
        "ctrl-idea",
        "Original Title",
        IdeaState::InDiscussion,
    );
    let (status, _) = post_form(state, "/idea/ctrl-idea/rename", "title=Foo%0AEvil").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        store::read_idea(&vault_dir, "ctrl-idea")
            .unwrap()
            .frontmatter
            .title,
        "Original Title"
    );
}
