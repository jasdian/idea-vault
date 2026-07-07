//! Settings (live LLM controls): toggle the active backend and tune its params with no restart.
//! `GET /settings` renders the page; `POST /settings` writes the runtime [`LlmSettings`] via
//! `state.llm.set_settings` and re-renders the form with a saved confirmation. Effective on the
//! next message — the router reads settings per call (`ai::backend`).

use axum::extract::State;
use axum::Form;
use serde::Deserialize;

use crate::ai::LlmSettings;
use crate::app::AppState;
use crate::config::LlmBackendKind;
use crate::web::templates::{SettingsForm, SettingsPage};
use crate::web::WebError;

fn form_view(state: &AppState, saved: bool) -> SettingsForm {
    let s = state.llm.settings();
    SettingsForm {
        is_ollama: s.backend == LlmBackendKind::Ollama,
        ollama_model: state.config.ollama_model.clone(),
        temperature: format!("{:.2}", s.temperature),
        claude_model: s.claude_model.clone(),
        effort: s.claude_effort.clone(),
        auto_compact: s.auto_compact,
        compact_threshold: format!("{:.2}", s.compact_threshold),
        saved,
    }
}

/// `GET /settings` — the live LLM controls page.
pub async fn settings_page(State(state): State<AppState>) -> Result<SettingsPage, WebError> {
    use askama::Template as _;
    let form_html = form_view(&state, false)
        .render()
        .map_err(|e| WebError::Internal(format!("template render: {e}")))?;
    Ok(SettingsPage { form_html })
}

#[derive(Debug, Deserialize)]
pub struct SettingsUpdate {
    #[serde(default)]
    pub backend: String,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub claude_model: String,
    #[serde(default)]
    pub effort: String,
    /// An unchecked checkbox is omitted from the form body ⇒ `false` via `serde(default)`.
    #[serde(default)]
    pub auto_compact: bool,
    #[serde(default)]
    pub compact_threshold: Option<f32>,
}

/// `POST /settings` — apply the change to the runtime settings and re-render the form.
pub async fn update_settings(
    State(state): State<AppState>,
    Form(form): Form<SettingsUpdate>,
) -> Result<SettingsForm, WebError> {
    let mut s = state.llm.settings();
    s.backend = match form.backend.as_str() {
        "claude-code" => LlmBackendKind::ClaudeCode,
        "ollama" => LlmBackendKind::Ollama,
        other => return Err(WebError::BadRequest(format!("unknown backend: {other}"))),
    };
    if let Some(t) = form.temperature {
        s.temperature = t.clamp(0.0, 2.0);
    }
    s.claude_model = form.claude_model.trim().to_string();
    let effort = form.effort.trim();
    if matches!(effort, "low" | "medium" | "high") {
        s.claude_effort = effort.to_string();
    }
    // Auto-compact (docs/adr/0012): the checkbox drives the toggle (absent ⇒ off); the threshold
    // is clamped to the supported band, defaulting when the field is absent.
    s.auto_compact = form.auto_compact;
    s.compact_threshold = form.compact_threshold.unwrap_or(0.80).clamp(0.5, 0.95);
    state.llm.set_settings(LlmSettings { ..s });
    tracing::info!(backend = ?state.llm.settings().backend, "llm settings updated");

    Ok(form_view(&state, true))
}
