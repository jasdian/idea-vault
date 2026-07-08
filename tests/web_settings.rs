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
async fn non_finite_temperature_and_threshold_are_rejected() {
    // `<input type=number>` blocks this in a browser, but curl doesn't: f32::clamp passes NaN
    // through unchanged, so without a finiteness filter `temperature=nan` would poison every
    // subsequent Ollama call and a NaN threshold would make the auto-compact gate always fire.
    let (state, _vault) = test_state();
    let before = state.llm.settings();
    let (status, _) = post_form(
        state.clone(),
        "/settings",
        "backend=ollama&temperature=nan&compact_threshold=NaN",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let after = state.llm.settings();
    assert_eq!(
        after.temperature, before.temperature,
        "NaN keeps the current temperature"
    );
    assert!(after.compact_threshold.is_finite());
    assert_eq!(
        after.compact_threshold, 0.80,
        "NaN threshold falls back to the default"
    );

    // Infinity is likewise non-finite — filtered, not clamped to the band edge.
    let (status, _) = post_form(
        state.clone(),
        "/settings",
        "backend=ollama&temperature=inf&compact_threshold=-inf",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let after = state.llm.settings();
    assert_eq!(after.temperature, before.temperature);
    assert_eq!(after.compact_threshold, 0.80);
}

#[tokio::test]
async fn settings_page_shows_context_window_controls_with_auto_default() {
    let (state, _vault) = test_state();
    let (_, body) = get(state, "/settings").await;
    assert!(body.contains("name=\"ollama_ctx_tokens\""));
    assert!(body.contains("name=\"claude_ctx_tokens\""));
    assert!(body.contains("0 = auto (derived from the model)"));
    // Cold cache + no override: the effective hint shows the 8192-token fallback.
    assert!(body.contains("effective now: 8192 tokens"));
}

#[tokio::test]
async fn context_tokens_round_trip_clamp_and_zero_means_auto() {
    let (state, _vault) = test_state();
    // A plain value round-trips and is live in the settings.
    let (status, body) = post_form(
        state.clone(),
        "/settings",
        "backend=ollama&ollama_ctx_tokens=16384&claude_ctx_tokens=0",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("name=\"ollama_ctx_tokens\" min=\"0\" value=\"16384\""));
    assert_eq!(state.llm.settings().ollama_ctx_tokens, 16_384);
    // The override drives the effective window immediately.
    assert!(body.contains("effective now: 16384 tokens"));

    // Nonzero values are clamped into 1024..=2_000_000; 0 stays 0 (auto).
    let (_, body) = post_form(
        state.clone(),
        "/settings",
        "backend=ollama&ollama_ctx_tokens=12&claude_ctx_tokens=99999999",
    )
    .await;
    assert!(body.contains("name=\"ollama_ctx_tokens\" min=\"0\" value=\"1024\""));
    assert!(body.contains("name=\"claude_ctx_tokens\" min=\"0\" value=\"2000000\""));

    let (_, body) = post_form(
        state.clone(),
        "/settings",
        "backend=ollama&ollama_ctx_tokens=0&claude_ctx_tokens=0",
    )
    .await;
    assert!(body.contains("name=\"ollama_ctx_tokens\" min=\"0\" value=\"0\""));
    assert_eq!(state.llm.settings().ollama_ctx_tokens, 0);
    assert!(
        body.contains("effective now: 8192 tokens"),
        "back to auto: the fallback window again"
    );
}

#[tokio::test]
async fn absent_context_tokens_fields_keep_the_current_values() {
    let (state, _vault) = test_state();
    let mut s = state.llm.settings();
    s.ollama_ctx_tokens = 4096;
    s.claude_ctx_tokens = 64_000;
    state.llm.set_settings(s);

    // A form body without the ctx fields (e.g. an older cached page) must not zero them.
    let (status, _) = post_form(state.clone(), "/settings", "backend=ollama").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(state.llm.settings().ollama_ctx_tokens, 4096);
    assert_eq!(state.llm.settings().claude_ctx_tokens, 64_000);
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
