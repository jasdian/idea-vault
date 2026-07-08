//! Web handler tests for R18–R20 (docs/adr/0015): the extract-knowledge background job (with
//! the opt-in HTML report), the artifacts panel, and per-file view/delete — mock Ollama only.

mod support;

use axum::http::StatusCode;
use chrono::{TimeZone, Utc};
use idea_vault::domain::{Idea, IdeaFrontmatter, IdeaState};
use idea_vault::vault::store;
use support::web::{get, poll_until, post_form, test_state, test_state_with_ollama};
use support::{spawn, spawn_sequence, ChatScript};

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
    store::append_turn(vault, "movable", "user", "harvest it").unwrap();
}

/// Five distinct lens outputs + one synthesis, mapped deterministically by K=1 FIFO ordering.
fn lens_scripts() -> Vec<ChatScript> {
    vec![
        ChatScript::Tokens(vec!["decisionfact".into()]),
        ChatScript::Tokens(vec!["durablefact".into()]),
        ChatScript::Tokens(vec!["questionfact".into()]),
        ChatScript::Tokens(vec!["riskfact".into()]),
        ChatScript::Tokens(vec!["actionfact".into()]),
        ChatScript::Tokens(vec!["convergedsummary".into()]),
    ]
}

fn artifact_files(vault: &std::path::Path) -> Vec<String> {
    store::list_artifact_files(vault, "movable")
        .unwrap()
        .into_iter()
        .map(|f| format!("{}.{}", f.slug, f.ext.as_str()))
        .collect()
}

#[tokio::test]
async fn extract_persists_artifacts_and_synthesis_without_html_by_default() {
    let mock = spawn_sequence(&["llama3.2"], lens_scripts()).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::InDiscussion);

    let (status, _) = post_form(state.clone(), "/idea/movable/extract", "").await;
    assert_eq!(status, StatusCode::OK);
    let body = poll_until(state.clone(), "/idea/movable/pending", "foil · knowledge").await;
    assert!(body.contains("convergedsummary"));
    // The completed transcript response carries the artifacts panel as an out-of-band swap, so
    // the harvested files surface without a page reload.
    assert!(
        body.contains(r#"id="artifacts" hx-swap-oob="true""#),
        "OOB artifacts panel missing: {body}"
    );
    assert!(body.contains("Key decisions"));

    // 5 findings + 1 synthesis on disk, no .html (checkbox not ticked).
    let files = artifact_files(&vault_dir);
    assert_eq!(files.len(), 6, "files: {files:?}");
    assert!(files.iter().all(|f| f.ends_with(".md")));
    assert!(files.iter().any(|f| f.contains("key-decisions")));
    assert!(files.iter().any(|f| f.contains("synthesis")));

    // Findings live in artifacts, not the transcript; the synthesis is the only new turn.
    let convo = store::read_conversation(&vault_dir, "movable").unwrap();
    assert!(convo.contains("## assistant (knowledge)\nconvergedsummary\n"));
    assert!(!convo.contains("decisionfact"));

    // The FTS index covers the artifact content end-to-end (R8 search hits it).
    let (_, results) = get(state.clone(), "/search?q=decisionfact").await;
    assert!(results.contains("movable"), "search miss: {results}");

    // The idea page now shows the artifacts panel; the moves row has no extract-* chips.
    let (_, page) = get(state, "/idea/movable").await;
    assert!(page.contains(r#"id="artifacts""#));
    assert!(page.contains("Key decisions"));
    assert!(
        page.contains("extract knowledge"),
        "the R18 chip is present"
    );
    assert!(!page.contains("/skill/extract-"), "no lens move chips");
}

#[tokio::test]
async fn extract_with_html_true_writes_a_standalone_report() {
    let mock = spawn_sequence(&["llama3.2"], lens_scripts()).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::InDiscussion);

    let (status, _) = post_form(state.clone(), "/idea/movable/extract", "html=true").await;
    assert_eq!(status, StatusCode::OK);
    poll_until(state.clone(), "/idea/movable/pending", "foil · knowledge").await;

    let files = artifact_files(&vault_dir);
    let report = files
        .iter()
        .find(|f| f.ends_with(".html"))
        .unwrap_or_else(|| panic!("no report in {files:?}"));
    assert!(report.contains("report"));

    let raw =
        store::read_artifact_html(&vault_dir, "movable", report.trim_end_matches(".html")).unwrap();
    assert!(raw.starts_with("<!DOCTYPE html"));
    // Full report: the synthesis section precedes the per-lens findings sections.
    let synthesis_at = raw.find("convergedsummary").expect("synthesis in report");
    let finding_at = raw.find("decisionfact").expect("finding in report");
    assert!(synthesis_at < finding_at, "summary first, then findings");
    assert!(raw.contains("Key decisions"));

    // Served raw through R19 — but locked down: a vault .html is owner-editable, so the
    // response must carry the script-blocking CSP (defense in depth).
    let app = idea_vault::app::build_router(state);
    let resp = tower::ServiceExt::oneshot(
        app,
        axum::http::Request::builder()
            .uri(format!("/idea/movable/artifact/{report}"))
            .body(axum::body::Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("content-security-policy")
            .expect("CSP header on raw .html")
            .to_str()
            .unwrap(),
        "default-src 'none'; style-src 'unsafe-inline'; img-src data:"
    );
    assert_eq!(
        resp.headers()
            .get("x-content-type-options")
            .expect("nosniff header")
            .to_str()
            .unwrap(),
        "nosniff"
    );
    let served = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .unwrap()
        .to_bytes();
    assert_eq!(String::from_utf8(served.to_vec()).unwrap(), raw);
}

#[tokio::test]
async fn extract_guards_states_and_missing_ideas() {
    let (state, vault_dir) = test_state();
    seed(&vault_dir, IdeaState::Draft);

    let (status, _) = post_form(state.clone(), "/idea/movable/extract", "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "Draft cannot extract");

    let (status, _) = post_form(state.clone(), "/idea/ghost/extract", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    seed(&vault_dir, IdeaState::Stored);
    let (status, _) = post_form(state, "/idea/movable/extract", "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "Stored cannot extract");
}

#[tokio::test]
async fn second_extract_while_running_shares_the_job_slot() {
    // Every call holds its response open, so the first run is still in flight when the second
    // POST lands — it must join (transcript + indicator), not start a second swarm.
    let mock = spawn(
        &["llama3.2"],
        ChatScript::TokensAfterDelay {
            tokens: vec!["slowfact".into()],
            delay_ms: 60,
        },
    )
    .await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::InDiscussion);

    let (status, _) = post_form(state.clone(), "/idea/movable/extract", "").await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = post_form(state.clone(), "/idea/movable/extract", "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("foil-pending"), "joins the running job");

    poll_until(state, "/idea/movable/pending", "foil · knowledge").await;
    assert_eq!(mock.chat_bodies().len(), 6, "one run, not two");
}

#[tokio::test]
async fn view_artifact_renders_md_and_rejects_hostile_names() {
    let mock = spawn_sequence(&["llama3.2"], lens_scripts()).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::InDiscussion);
    post_form(state.clone(), "/idea/movable/extract", "").await;
    poll_until(state.clone(), "/idea/movable/pending", "foil · knowledge").await;

    let md = artifact_files(&vault_dir)
        .into_iter()
        .find(|f| f.contains("key-decisions"))
        .unwrap();
    let (status, page) = get(state.clone(), &format!("/idea/movable/artifact/{md}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Key decisions"));
    assert!(page.contains("decisionfact"));
    assert!(page.contains("← back to Movable"));

    for hostile in [
        "..%2F..%2Fetc%2Fpasswd.md",
        "foo.txt",
        "nodot",
        "UPPER.md",
        "nope.md",
        "nope.html",
    ] {
        let (status, _) = get(state.clone(), &format!("/idea/movable/artifact/{hostile}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "should 404: {hostile}");
    }
}

#[tokio::test]
async fn delete_artifact_removes_the_file_and_its_search_rows() {
    let mock = spawn_sequence(&["llama3.2"], lens_scripts()).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    seed(&vault_dir, IdeaState::InDiscussion);
    post_form(state.clone(), "/idea/movable/extract", "").await;
    poll_until(state.clone(), "/idea/movable/pending", "foil · knowledge").await;

    let md = artifact_files(&vault_dir)
        .into_iter()
        .find(|f| f.contains("key-decisions"))
        .unwrap();
    let (_, results) = get(state.clone(), "/search?q=decisionfact").await;
    assert!(results.contains("movable"));

    let (status, panel) = post_form(
        state.clone(),
        &format!("/idea/movable/artifact/{md}/delete"),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(panel.contains(r#"id="artifacts""#), "panel returned");
    assert!(!panel.contains(&md), "deleted file no longer listed");
    assert!(!vault_dir.join("movable/artifacts").join(&md).exists());

    // Its FTS rows went with it (the delete reindexes); other artifacts still hit.
    let (_, results) = get(state.clone(), "/search?q=decisionfact").await;
    assert!(!results.contains("movable"), "stale hit: {results}");
    let (_, results) = get(state.clone(), "/search?q=durablefact").await;
    assert!(results.contains("movable"));

    // Deleting it again is a 404, not a silent success.
    let (status, _) = post_form(state, &format!("/idea/movable/artifact/{md}/delete"), "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
