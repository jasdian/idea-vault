//! Web handler tests for the tags pipeline's new faucets (audit fix: tags existed in the data
//! model, the index, and search, but nothing ever populated them): the inline editor
//! (`POST /idea/{slug}/tags`), the chips on rows/idea pages, and the `?tag=` list filter that
//! finally routes `ideas_with_tag`.

mod support;

use axum::http::StatusCode;
use chrono::{TimeZone, Utc};
use idea_vault::domain::{Idea, IdeaFrontmatter, IdeaState};
use idea_vault::vault::store;
use support::web::{get, post_form, test_state};

fn seed(vault: &std::path::Path, slug: &str, tags: Vec<String>) {
    store::write_idea(
        vault,
        &Idea {
            frontmatter: IdeaFrontmatter {
                title: format!("Idea {slug}"),
                slug: slug.into(),
                state: IdeaState::InDiscussion,
                tags,
                created: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                updated: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
            },
            body: "Body.\n".into(),
        },
    )
    .unwrap();
}

#[tokio::test]
async fn set_tags_slugifies_persists_and_renders_chips() {
    let (state, vault_dir) = test_state();
    seed(&vault_dir, "taggable", vec![]);

    let (status, body) = post_form(
        state,
        "/idea/taggable/tags",
        "tags=Trading Tools, MVP!,  , trading tools",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Slugified, deduped chips linking to the list filter.
    assert!(body.contains(r#"href="/?tag=trading-tools""#), "{body}");
    assert!(body.contains("#mvp"));
    let idea = store::read_idea(&vault_dir, "taggable").unwrap();
    assert_eq!(idea.frontmatter.tags, vec!["trading-tools", "mvp"]);
    assert_eq!(
        idea.frontmatter.state,
        IdeaState::InDiscussion,
        "state untouched"
    );
}

#[tokio::test]
async fn empty_input_clears_tags_and_busy_idea_refuses() {
    let (state, vault_dir) = test_state();
    seed(&vault_dir, "taggable", vec!["old".into()]);

    // Replace semantics: an empty save clears the set.
    let (status, _) = post_form(state.clone(), "/idea/taggable/tags", "tags=").await;
    assert_eq!(status, StatusCode::OK);
    assert!(store::read_idea(&vault_dir, "taggable")
        .unwrap()
        .frontmatter
        .tags
        .is_empty());

    // Same whole-file-write race as rename: a running job blocks the edit with a readable 400.
    assert!(idea_vault::web::jobs::try_claim(&state.jobs, "taggable"));
    let (status, body) = post_form(state.clone(), "/idea/taggable/tags", "tags=x").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("run is in progress"));
    idea_vault::web::jobs::mark_done(&state.jobs, "taggable");

    let (status, _) = post_form(state, "/idea/ghost/tags", "tags=x").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn list_filters_by_tag_and_rows_carry_chips() {
    let (state, vault_dir) = test_state();
    seed(&vault_dir, "tagged", vec!["risk".into()]);
    seed(&vault_dir, "untagged", vec![]);
    // The list reads the index, not the files — reindex first (the editor route does this
    // itself; here we seeded directly on disk).
    let (status, _) = post_form(state.clone(), "/admin/reindex", "").await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = get(state.clone(), "/?tag=risk").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("/idea/tagged"));
    assert!(!body.contains("/idea/untagged"), "filter narrows the list");
    assert!(body.contains("filtered by") && body.contains("clear"));

    // Unfiltered: both rows, and the tagged row carries its chip.
    let (_, body) = get(state, "/").await;
    assert!(body.contains("/idea/untagged"));
    assert!(
        body.contains(r#"href="/?tag=risk""#),
        "row chip links to the filter"
    );
}

#[tokio::test]
async fn idea_page_shows_tag_row_and_editor() {
    let (state, vault_dir) = test_state();
    seed(&vault_dir, "taggable", vec!["alpha".into()]);
    let (status, body) = get(state, "/idea/taggable").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(r#"id="idea-tags-row""#));
    assert!(body.contains("#alpha"));
    assert!(body.contains(r#"hx-post="/idea/taggable/tags""#));
}
