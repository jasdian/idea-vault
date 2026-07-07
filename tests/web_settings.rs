//! The live LLM settings page: GET renders the form with current values; POST toggles the backend
//! and tunes params at runtime, reflected immediately by /admin/health.

mod support;

use axum::http::StatusCode;
use support::web::{get, post_form, test_state};

#[tokio::test]
async fn settings_page_renders_the_form() {
    let (state, _vault) = test_state();
    let (status, body) = get(state, "/settings").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("name=\"backend\""));
    assert!(body.contains("local temperature") && body.contains("claude effort"));
    // Ollama is the default active backend.
    assert!(body.contains("value=\"ollama\" checked"));
}

#[tokio::test]
async fn saving_toggles_the_backend_live() {
    let (state, _vault) = test_state();
    // Health starts on ollama.
    let (_, h0) = get(state.clone(), "/admin/health").await;
    assert!(h0.contains("\"backend\":\"ollama\""));

    // Toggle to claude-code with an effort + temperature.
    let (status, body) = post_form(
        state.clone(),
        "/settings",
        "backend=claude-code&temperature=0.30&claude_model=sonnet&effort=high",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Saved"));
    assert!(body.contains("value=\"claude-code\" checked"));
    assert!(body.contains("sonnet"));

    // The change is live: health now reports claude-code.
    let (_, h1) = get(state, "/admin/health").await;
    assert!(h1.contains("\"backend\":\"claude-code\""));
}

#[tokio::test]
async fn unknown_backend_is_rejected() {
    let (state, _vault) = test_state();
    let (status, _) = post_form(state, "/settings", "backend=gpt5&temperature=0.7").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn settings_page_shows_auto_compact_controls_checked_by_default() {
    let (state, _vault) = test_state();
    let (_, body) = get(state, "/settings").await;
    assert!(body.contains("name=\"auto_compact\""));
    assert!(body.contains("name=\"compact_threshold\""));
    // Default is on.
    assert!(body.contains("name=\"auto_compact\" value=\"true\" checked"));
}

#[tokio::test]
async fn saving_round_trips_and_clamps_the_compact_threshold() {
    let (state, _vault) = test_state();
    // In-range threshold round-trips; a checked box keeps auto-compact on.
    let (status, body) = post_form(
        state.clone(),
        "/settings",
        "backend=ollama&auto_compact=true&compact_threshold=0.90",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("value=\"0.90\""));
    assert!(body.contains("name=\"auto_compact\" value=\"true\" checked"));

    // Out-of-range clamps to the 0.5..=0.95 band.
    let (_, body) = post_form(
        state.clone(),
        "/settings",
        "backend=ollama&auto_compact=true&compact_threshold=0.99",
    )
    .await;
    assert!(body.contains("value=\"0.95\""), "threshold clamped to 0.95");
}

#[tokio::test]
async fn unchecked_auto_compact_box_turns_it_off() {
    let (state, _vault) = test_state();
    // An unchecked checkbox is simply omitted from the form body ⇒ auto_compact = false.
    let (status, body) =
        post_form(state, "/settings", "backend=ollama&compact_threshold=0.80").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !body.contains("name=\"auto_compact\" value=\"true\" checked"),
        "omitting the checkbox disables auto-compact"
    );
}
