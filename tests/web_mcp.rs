//! Web handler tests for the MCP server management page (`GET`/`POST /mcp`): empty state, the
//! add/toggle/delete round trip against the persisted registry file, validation rejection, a
//! probe against an endpoint that refuses the connection (offline-safe — no live MCP server), and
//! the edit-in-place flow (edit form never echoes the token, blank keeps it, the checkbox clears
//! it, an invalid url on update is a 400).

mod support;

use axum::http::StatusCode;
use support::refused_url;
use support::web::{get, post_form, test_state};

#[tokio::test]
async fn mcp_page_renders_empty_state() {
    let (state, _vault) = test_state();
    let (status, body) = get(state, "/mcp").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("No MCP servers configured yet"));
    assert!(body.contains("add a server"));
    assert!(body.contains("name=\"name\""));
    assert!(body.contains("name=\"url\""));
    assert!(body.contains("name=\"bearer_token\""));
}

#[tokio::test]
async fn add_appears_in_list_and_persists_without_echoing_the_token() {
    let (state, _vault) = test_state();
    let (status, body) = post_form(
        state.clone(),
        "/mcp/add",
        "name=tracker&url=https%3A%2F%2Fmcp.example%2Frpc&bearer_token=super-secret-token",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("tracker"));
    assert!(body.contains("mcp.example/rpc"));
    assert!(body.contains("token set"));
    assert!(
        !body.contains("super-secret-token"),
        "the bearer token must never be echoed back into the page"
    );

    // Persisted to the config file on disk (app config, not vault truth) — the token IS stored
    // there (it has to be, to be usable), just never rendered.
    let raw = std::fs::read_to_string(&state.config.mcp_config_path).expect("config file exists");
    assert!(raw.contains("tracker"));
    assert!(raw.contains("super-secret-token"));
}

#[tokio::test]
async fn duplicate_and_invalid_add_are_rejected_with_400() {
    let (state, _vault) = test_state();
    let (status, _) = post_form(
        state.clone(),
        "/mcp/add",
        "name=tracker&url=https%3A%2F%2Fmcp.example%2Frpc&bearer_token=",
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Duplicate name.
    let (status, body) = post_form(
        state.clone(),
        "/mcp/add",
        "name=tracker&url=https%3A%2F%2Fmcp.example%2Fother&bearer_token=",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("already exists"));

    // Invalid name (uppercase, outside the slug alphabet).
    let (status, _) = post_form(
        state.clone(),
        "/mcp/add",
        "name=Has+Caps&url=https%3A%2F%2Fmcp.example%2Frpc&bearer_token=",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Invalid URL (not http/https).
    let (status, _) = post_form(
        state.clone(),
        "/mcp/add",
        "name=other&url=ftp%3A%2F%2Fmcp.example&bearer_token=",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Nothing invalid actually landed in the registry.
    let (_, body) = get(state, "/mcp").await;
    assert!(!body.contains("Has Caps"));
    assert!(!body.contains("ftp://"));
}

#[tokio::test]
async fn toggle_flips_enabled_and_persists() {
    let (state, _vault) = test_state();
    post_form(
        state.clone(),
        "/mcp/add",
        "name=tracker&url=https%3A%2F%2Fmcp.example%2Frpc&bearer_token=",
    )
    .await;

    // Freshly added servers default to enabled.
    let raw = std::fs::read_to_string(&state.config.mcp_config_path).unwrap();
    assert!(raw.contains("\"enabled\": true"));

    let (status, body) = post_form(state.clone(), "/mcp/tracker/toggle", "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(">disabled<"),
        "row now shows the disabled state; body was:\n{body}"
    );

    let raw = std::fs::read_to_string(&state.config.mcp_config_path).unwrap();
    assert!(raw.contains("\"enabled\": false"), "toggle persisted off");

    // Toggle again flips back on.
    let (status, body) = post_form(state.clone(), "/mcp/tracker/toggle", "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(">enabled<"));
    let raw = std::fs::read_to_string(&state.config.mcp_config_path).unwrap();
    assert!(
        raw.contains("\"enabled\": true"),
        "second toggle persisted on"
    );
}

#[tokio::test]
async fn toggle_unknown_server_is_404() {
    let (state, _vault) = test_state();
    let (status, _) = post_form(state, "/mcp/ghost/toggle", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_removes_from_list_and_disk() {
    let (state, _vault) = test_state();
    post_form(
        state.clone(),
        "/mcp/add",
        "name=tracker&url=https%3A%2F%2Fmcp.example%2Frpc&bearer_token=",
    )
    .await;
    let (_, body) = get(state.clone(), "/mcp").await;
    assert!(body.contains("tracker"));

    let (status, body) = post_form(state.clone(), "/mcp/tracker/delete", "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("mcp-row-tracker"));
    assert!(body.contains("No MCP servers configured yet"));

    let raw = std::fs::read_to_string(&state.config.mcp_config_path).unwrap();
    assert!(!raw.contains("tracker"));
}

/// Regression for the rejected first pass: a server row must render a delete control that is
/// always visible (not the hover-only `.mem-del` opacity trick, which read as "missing" on this
/// page), plus a visible edit control.
#[tokio::test]
async fn list_html_contains_always_visible_delete_and_edit_controls() {
    let (state, _vault) = test_state();
    post_form(
        state.clone(),
        "/mcp/add",
        "name=tracker&url=https%3A%2F%2Fmcp.example%2Frpc&bearer_token=",
    )
    .await;
    let (_, body) = get(state, "/mcp").await;

    // The delete action must exist and must NOT use the hover-only memory-panel class.
    assert!(
        body.contains("/mcp/tracker/delete"),
        "delete action missing from the row"
    );
    assert!(
        body.contains("chip--danger"),
        "delete must render as an always-visible danger chip, not a hover-only control"
    );
    assert!(
        !body.contains("class=\"mem-del\""),
        "delete must not reuse the hover-only-opacity memory-panel pattern"
    );

    // The edit action must exist too (previously entirely absent).
    assert!(
        body.contains("/mcp/tracker/edit"),
        "edit action missing from the row"
    );
}

#[tokio::test]
async fn edit_form_renders_without_echoing_the_stored_token() {
    let (state, _vault) = test_state();
    post_form(
        state.clone(),
        "/mcp/add",
        "name=tracker&url=https%3A%2F%2Fmcp.example%2Frpc&bearer_token=super-secret-token",
    )
    .await;

    let (status, body) = get(state, "/mcp/tracker/edit").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("mcp.example/rpc"), "url is prefilled");
    assert!(
        !body.contains("super-secret-token"),
        "the stored token must never be echoed into the edit form"
    );
    assert!(
        body.contains("leave blank to keep current"),
        "placeholder communicates blank == keep, given a token is already set"
    );
    assert!(
        body.contains("name=\"clear_token\""),
        "a clear-token checkbox must be present"
    );
    // Name is immutable — no editable name field in the edit form.
    assert!(!body.contains("name=\"name\""));
}

#[tokio::test]
async fn edit_unknown_server_is_404() {
    let (state, _vault) = test_state();
    let (status, _) = get(state, "/mcp/ghost/edit").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn view_row_swaps_back_without_mutating_anything() {
    let (state, _vault) = test_state();
    post_form(
        state.clone(),
        "/mcp/add",
        "name=tracker&url=https%3A%2F%2Fmcp.example%2Frpc&bearer_token=",
    )
    .await;
    let (status, body) = get(state, "/mcp/tracker/view").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("mcp-row-tracker"));
    assert!(body.contains("mcp.example/rpc"));
}

#[tokio::test]
async fn update_changes_url_on_disk() {
    let (state, _vault) = test_state();
    post_form(
        state.clone(),
        "/mcp/add",
        "name=tracker&url=https%3A%2F%2Fmcp.example%2Frpc&bearer_token=",
    )
    .await;

    let (status, body) = post_form(
        state.clone(),
        "/mcp/tracker/update",
        "url=https%3A%2F%2Fmcp.example%2Fv2&bearer_token=&clear_token=",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("mcp.example/v2"));

    let raw = std::fs::read_to_string(&state.config.mcp_config_path).unwrap();
    assert!(raw.contains("mcp.example/v2"));
    assert!(!raw.contains("mcp.example/rpc"));
}

#[tokio::test]
async fn update_blank_token_keeps_stored_token_but_explicit_clear_erases_it() {
    let (state, _vault) = test_state();
    post_form(
        state.clone(),
        "/mcp/add",
        "name=tracker&url=https%3A%2F%2Fmcp.example%2Frpc&bearer_token=orig-token",
    )
    .await;

    // Blank token field, checkbox absent ⇒ token untouched.
    post_form(
        state.clone(),
        "/mcp/tracker/update",
        "url=https%3A%2F%2Fmcp.example%2Frpc&bearer_token=",
    )
    .await;
    let raw = std::fs::read_to_string(&state.config.mcp_config_path).unwrap();
    assert!(
        raw.contains("orig-token"),
        "a blank token field must keep the existing token"
    );

    // clear_token present ⇒ token erased even though the field is still blank.
    post_form(
        state.clone(),
        "/mcp/tracker/update",
        "url=https%3A%2F%2Fmcp.example%2Frpc&bearer_token=&clear_token=true",
    )
    .await;
    let raw = std::fs::read_to_string(&state.config.mcp_config_path).unwrap();
    assert!(
        !raw.contains("orig-token"),
        "the clear-token checkbox must erase the stored token"
    );
}

#[tokio::test]
async fn update_rejects_invalid_url_with_400() {
    let (state, _vault) = test_state();
    post_form(
        state.clone(),
        "/mcp/add",
        "name=tracker&url=https%3A%2F%2Fmcp.example%2Frpc&bearer_token=",
    )
    .await;

    let (status, body) = post_form(
        state.clone(),
        "/mcp/tracker/update",
        "url=ftp%3A%2F%2Fbad&bearer_token=",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("invalid server url"));

    // The original url must survive the rejected update.
    let raw = std::fs::read_to_string(&state.config.mcp_config_path).unwrap();
    assert!(raw.contains("mcp.example/rpc"));
}

#[tokio::test]
async fn update_unknown_server_is_404() {
    let (state, _vault) = test_state();
    let (status, _) = post_form(
        state,
        "/mcp/ghost/update",
        "url=https%3A%2F%2Fmcp.example%2Frpc&bearer_token=",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_unknown_server_is_404() {
    let (state, _vault) = test_state();
    let (status, _) = post_form(state, "/mcp/ghost/delete", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn probe_unknown_server_is_404() {
    let (state, _vault) = test_state();
    let (status, _) = post_form(state, "/mcp/ghost/probe", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn probe_against_a_refusing_endpoint_renders_a_readable_error_not_a_panic() {
    let (state, _vault) = test_state();
    let url = refused_url().await;
    let (status, _) = post_form(
        state.clone(),
        "/mcp/add",
        &format!("name=tracker&url={}&bearer_token=", urlencoding_lite(&url)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = post_form(state, "/mcp/tracker/probe", "").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a transport failure is content, not a 5xx"
    );
    assert!(body.contains("mcp-status-tracker"));
    assert!(body.contains("mcp__status--error"));
    assert!(!body.contains("tools · ok"));
}

/// Minimal `application/x-www-form-urlencoded` value-escaping for the one case these tests need
/// (a `http://127.0.0.1:PORT` URL) — avoids pulling in a URL-encoding crate dependency just for
/// tests.
fn urlencoding_lite(s: &str) -> String {
    s.replace(':', "%3A").replace('/', "%2F")
}
