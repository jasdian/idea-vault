//! Web handler tests for the Sources page (`GET`/`POST /sources`) and the per-idea attach row
//! (`POST /idea/{slug}/sources`): empty state, the add/edit/delete round trip against the
//! persisted registry file (bare mode — the harness sets no sources mount, so a registered
//! tempdir reads as "readable"), validation rejection, and the frontmatter `sources:` round trip
//! including the newly-attached-names-must-be-registered rule and the busy-idea guard.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; HTC-6 binds shipping code"
)]

mod support;

use axum::http::StatusCode;
use chrono::{TimeZone, Utc};
use idea_vault::app::AppState;
use idea_vault::domain::{Idea, IdeaFrontmatter, IdeaState};
use idea_vault::vault::store;
use support::web::{get, post_form, test_state};

fn seed(vault: &std::path::Path, slug: &str, sources: Vec<String>) {
    store::write_idea(
        vault,
        &Idea {
            frontmatter: IdeaFrontmatter {
                title: format!("Idea {slug}"),
                slug: slug.into(),
                state: IdeaState::InDiscussion,
                tags: vec![],
                sources,
                created: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                updated: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                extra: Default::default(),
            },
            body: "Body.\n".into(),
        },
    )
    .unwrap();
}

/// Minimal `application/x-www-form-urlencoded` value-escaping for the one case these tests need
/// (an absolute tempdir path) — same rationale as `web_mcp.rs`'s helper: no URL-encoding crate
/// dependency just for tests.
fn urlencoding_lite(s: &str) -> String {
    s.replace(':', "%3A").replace('/', "%2F")
}

/// Register `name` → a fresh tempdir through the real add route; return the (kept) dir path.
async fn add_source(state: &AppState, name: &str) -> std::path::PathBuf {
    let dir = tempfile::tempdir().expect("source dir");
    let path = dir.keep();
    let (status, _) = post_form(
        state.clone(),
        "/sources/add",
        &format!(
            "name={name}&host_path={}",
            urlencoding_lite(path.to_str().unwrap())
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "registering '{name}' must succeed");
    path
}

#[tokio::test]
async fn sources_page_renders_empty_state_in_bare_mode() {
    let (state, _vault) = test_state();
    let (status, body) = get(state, "/sources").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("no sources registered yet"), "{body}");
    assert!(body.contains("add a source"));
    assert!(body.contains("name=\"name\""));
    assert!(body.contains("name=\"host_path\""));
    // The harness sets no sources mount, so the panel explains bare mode, never the re-up flow.
    assert!(body.contains("bare mode"), "{body}");
    assert!(!body.contains("mount plan changed"));
}

#[tokio::test]
async fn add_appears_with_readable_status_and_persists() {
    let (state, _vault) = test_state();
    let path = add_source(&state, "refs").await;

    let (status, body) = get(state.clone(), "/sources").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("refs"));
    // Bare mode + an existing dir ⇒ the "readable" ok pill.
    assert!(body.contains(">readable<"), "{body}");
    assert!(body.contains("src__status--ok"), "{body}");
    // Always-visible edit + delete controls, like the MCP rows.
    assert!(body.contains("/sources/refs/edit"));
    assert!(body.contains("/sources/refs/delete"));

    // Persisted to the registry file on disk (app config, not vault truth).
    let raw =
        std::fs::read_to_string(&state.config.sources_config_path).expect("config file exists");
    assert!(raw.contains("refs"));
    assert!(raw.contains(path.to_str().unwrap()));
}

#[tokio::test]
async fn add_rejects_bad_name_and_relative_path_with_400() {
    let (state, _vault) = test_state();

    // Invalid name (uppercase, outside the slug alphabet).
    let (status, body) = post_form(
        state.clone(),
        "/sources/add",
        "name=Has+Caps&host_path=%2Fsrv%2Fdocs",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("invalid source name"), "{body}");

    // Relative path.
    let (status, body) = post_form(
        state.clone(),
        "/sources/add",
        "name=docs&host_path=relative%2Fdocs",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("absolute"), "{body}");

    // Nothing invalid landed in the registry.
    let (_, body) = get(state, "/sources").await;
    assert!(body.contains("no sources registered yet"));
}

#[tokio::test]
async fn edit_form_then_update_changes_the_path_on_disk() {
    let (state, _vault) = test_state();
    let old_path = add_source(&state, "refs").await;

    // The edit row prefills the current path; the name renders fixed, never as an input.
    let (status, body) = get(state.clone(), "/sources/refs/edit").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(old_path.to_str().unwrap()), "{body}");
    assert!(body.contains("name is fixed"), "{body}");
    assert!(!body.contains("name=\"name\""));

    let new_dir = tempfile::tempdir().unwrap().keep();
    let (status, body) = post_form(
        state.clone(),
        "/sources/refs/update",
        &format!("host_path={}", urlencoding_lite(new_dir.to_str().unwrap())),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(new_dir.to_str().unwrap()), "{body}");

    let raw = std::fs::read_to_string(&state.config.sources_config_path).unwrap();
    assert!(raw.contains(new_dir.to_str().unwrap()));
    assert!(!raw.contains(old_path.to_str().unwrap()));

    // A bad path on update is a 400 and the stored path survives.
    let (status, _) = post_form(
        state.clone(),
        "/sources/refs/update",
        "host_path=relative%2Fnope",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let raw = std::fs::read_to_string(&state.config.sources_config_path).unwrap();
    assert!(raw.contains(new_dir.to_str().unwrap()));

    // Unknown name is a stale panel ⇒ 404, not 400.
    let (status, _) = post_form(state, "/sources/ghost/update", "host_path=%2Fsrv%2Fx").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_removes_from_list_and_disk() {
    let (state, _vault) = test_state();
    add_source(&state, "refs").await;

    let (status, body) = post_form(state.clone(), "/sources/refs/delete", "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("src-row-refs"));
    assert!(body.contains("no sources registered yet"));

    let raw = std::fs::read_to_string(&state.config.sources_config_path).unwrap();
    assert!(!raw.contains("refs"));

    let (status, _) = post_form(state, "/sources/refs/delete", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn attach_and_detach_round_trip_through_frontmatter() {
    let (state, vault_dir) = test_state();
    seed(&vault_dir, "sourced", vec![]);
    add_source(&state, "refs").await;

    // Attach: the checkbox editor posts one `sources=<name>` pair per checked box.
    let (status, body) = post_form(state.clone(), "/idea/sourced/sources", "sources=refs").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("srcchip"), "{body}");
    assert!(body.contains("refs"));
    let idea = store::read_idea(&vault_dir, "sourced").unwrap();
    assert_eq!(idea.frontmatter.sources, vec!["refs"]);
    assert_eq!(
        idea.frontmatter.state,
        IdeaState::InDiscussion,
        "state untouched"
    );
    // The list is frontmatter truth, not index-only: the key is in idea.md itself.
    let raw = std::fs::read_to_string(vault_dir.join("sourced/idea.md")).unwrap();
    assert!(raw.contains("sources:"), "{raw}");

    // Detach: no checked boxes ⇒ an empty body ⇒ the set clears.
    let (status, _) = post_form(state.clone(), "/idea/sourced/sources", "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(store::read_idea(&vault_dir, "sourced")
        .unwrap()
        .frontmatter
        .sources
        .is_empty());
}

#[tokio::test]
async fn attaching_an_unregistered_name_is_400_but_an_existing_stale_name_persists() {
    let (state, vault_dir) = test_state();
    seed(&vault_dir, "sourced", vec!["ghost".into()]);
    add_source(&state, "refs").await;

    // Newly checked name unknown to the registry ⇒ 400, frontmatter untouched.
    let (status, body) = post_form(state.clone(), "/idea/sourced/sources", "sources=phantom").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("no source named 'phantom'"), "{body}");
    assert_eq!(
        store::read_idea(&vault_dir, "sourced")
            .unwrap()
            .frontmatter
            .sources,
        vec!["ghost"]
    );

    // But an ALREADY-attached stale name may persist alongside a fresh registered one —
    // frontmatter is truth; the owner may re-register 'ghost' later.
    let (status, _) = post_form(
        state.clone(),
        "/idea/sourced/sources",
        "sources=ghost&sources=refs",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        store::read_idea(&vault_dir, "sourced")
            .unwrap()
            .frontmatter
            .sources,
        vec!["ghost", "refs"]
    );

    // Invalid name shape is rejected regardless of registry state.
    let (status, _) = post_form(state.clone(), "/idea/sourced/sources", "sources=Has%20Caps").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = post_form(state, "/idea/missing/sources", "sources=refs").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn busy_idea_refuses_the_edit_with_a_readable_400() {
    let (state, vault_dir) = test_state();
    seed(&vault_dir, "sourced", vec![]);
    add_source(&state, "refs").await;

    // Same whole-file-write race as tags/rename: a running job blocks the edit.
    assert!(idea_vault::web::jobs::try_claim(&state.jobs, "sourced"));
    let (status, body) = post_form(state.clone(), "/idea/sourced/sources", "sources=refs").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("run is in progress"), "{body}");
    idea_vault::web::jobs::mark_done(&state.jobs, "sourced");

    // Frontmatter untouched by the refused edit.
    assert!(store::read_idea(&vault_dir, "sourced")
        .unwrap()
        .frontmatter
        .sources
        .is_empty());
}

#[tokio::test]
async fn idea_page_shows_the_chip_and_editor_after_attach() {
    let (state, vault_dir) = test_state();
    seed(&vault_dir, "sourced", vec![]);
    add_source(&state, "refs").await;
    post_form(state.clone(), "/idea/sourced/sources", "sources=refs").await;

    let (status, body) = get(state, "/idea/sourced").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(r#"id="idea-sources-row""#), "{body}");
    assert!(body.contains("srcchip"), "{body}");
    assert!(
        body.contains(r#"hx-post="/idea/sourced/sources""#),
        "{body}"
    );
    // The registered source's checkbox renders checked.
    assert!(body.contains(r#"value="refs" checked"#), "{body}");
}
