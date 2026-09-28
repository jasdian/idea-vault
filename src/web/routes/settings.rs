//! Settings (live LLM controls): toggle the active backend and tune its params with no restart.
//! `GET /settings` renders the page; `POST /settings` writes the runtime [`LlmSettings`] via
//! `state.llm.set_settings` and re-renders the form with a saved confirmation. Effective on the
//! next message — the router reads settings per call (`ai::backend`).

use axum::extract::{RawForm, State};
use serde::Deserialize;

use crate::ai::{LlmSettings, RoleProfile};
use crate::app::AppState;
use crate::concepts::agents::AgentRole;
use crate::config::LlmBackendKind;
use crate::web::templates::{RoleRow, SettingsForm, SettingsPage};
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
        ollama_ctx_tokens: s.ollama_ctx_tokens.to_string(),
        claude_ctx_tokens: s.claude_ctx_tokens.to_string(),
        effective_ctx: state.llm.context_window_tokens().to_string(),
        web_access: s.web_access,
        audit_findings: s.audit_findings,
        role_tuning: s.role_tuning,
        roles: AgentRole::ALL
            .iter()
            .map(|role| {
                let p = role_profile(&s, *role);
                RoleRow {
                    name: role.as_str(),
                    temperature: format!("{:.2}", p.temperature),
                    claude_model: p.claude_model,
                    effort: p.claude_effort,
                }
            })
            .collect(),
        saved,
    }
}

/// The role's live profile, or its built-in default when the map has none.
fn role_profile(s: &LlmSettings, role: AgentRole) -> RoleProfile {
    s.role_profiles
        .get(role.as_str())
        .cloned()
        .unwrap_or_else(|| role.default_profile())
}

/// A submitted temperature, finite and clamped into the Ollama band; `None` keeps the current value.
fn parse_temperature(raw: &str) -> Option<f32> {
    raw.trim()
        .parse::<f32>()
        .ok()
        .filter(|t| t.is_finite())
        .map(|t| t.clamp(0.0, 2.0))
}

/// Apply the `role_<name>_{temperature,model,effort}` fields to every role's profile: an absent
/// or invalid field keeps the current value; a blank model or effort means "inherit the global".
fn apply_role_fields(s: &mut LlmSettings, pairs: &[(String, String)]) {
    let field = |key: String| {
        pairs
            .iter()
            .rev()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.trim())
    };
    for role in AgentRole::ALL {
        let name = role.as_str();
        let mut p = role_profile(s, role);
        if let Some(t) = field(format!("role_{name}_temperature")).and_then(parse_temperature) {
            p.temperature = t;
        }
        if let Some(model) = field(format!("role_{name}_model")) {
            p.claude_model = model.to_string();
        }
        if let Some(effort) = field(format!("role_{name}_effort")) {
            if matches!(effort, "" | "low" | "medium" | "high") {
                p.claude_effort = effort.to_string();
            }
        }
        s.role_profiles.insert(name.to_string(), p);
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
    /// Web access (ADR-0017) — checkbox, same omitted-means-off contract as `auto_compact`.
    #[serde(default)]
    pub web_access: bool,
    /// Factored audit (docs/adr/0023) — checkbox, same omitted-means-off contract.
    #[serde(default)]
    pub audit_findings: bool,
    #[serde(default)]
    pub compact_threshold: Option<f32>,
    /// Per-backend context-window overrides in tokens; 0 = auto, absent = keep current.
    #[serde(default)]
    pub ollama_ctx_tokens: Option<usize>,
    #[serde(default)]
    pub claude_ctx_tokens: Option<usize>,
    /// Per-role call profiles (docs/adr/0026) — checkbox, same omitted-means-off contract. The
    /// per-role fields are read from the raw pairs by [`apply_role_fields`].
    #[serde(default)]
    pub role_tuning: bool,
}

/// Normalize a submitted context-window override: `0` stays `0` (auto), anything else is clamped
/// into the supported token band (same contract as the env-var initial values, `config.rs`).
fn clamp_ctx_tokens(raw: usize) -> usize {
    if raw == 0 {
        0
    } else {
        raw.clamp(crate::config::CTX_TOKENS_MIN, crate::config::CTX_TOKENS_MAX)
    }
}

/// `POST /settings` — apply the change to the runtime settings and re-render the form. The body
/// is read raw and parsed twice (typed fields, then the `role_*` pairs), so a malformed body is a
/// `400`, like the other `RawForm` routes.
pub async fn update_settings(
    State(state): State<AppState>,
    RawForm(body): RawForm,
) -> Result<SettingsForm, WebError> {
    let form: SettingsUpdate = serde_urlencoded::from_bytes(&body)
        .map_err(|e| WebError::BadRequest(format!("settings form: {e}")))?;
    let pairs: Vec<(String, String)> = serde_urlencoded::from_bytes(&body)
        .map_err(|e| WebError::BadRequest(format!("settings form: {e}")))?;
    let mut s = state.llm.settings();
    s.backend = match form.backend.as_str() {
        "claude-code" => LlmBackendKind::ClaudeCode,
        "ollama" => LlmBackendKind::Ollama,
        other => return Err(WebError::BadRequest(format!("unknown backend: {other}"))),
    };
    // NaN passes through `f32::clamp` unchanged, so non-finite input (`temperature=nan` via curl)
    // is filtered out — it keeps the current value rather than poisoning the process-wide settings.
    if let Some(t) = form.temperature.filter(|t| t.is_finite()) {
        s.temperature = t.clamp(0.0, 2.0);
    }
    s.claude_model = form.claude_model.trim().to_string();
    let effort = form.effort.trim();
    if matches!(effort, "low" | "medium" | "high") {
        s.claude_effort = effort.to_string();
    }
    // Auto-compact (docs/adr/0012): the checkbox drives the toggle (absent ⇒ off); the threshold
    // is clamped to the supported band, defaulting when the field is absent or non-finite (a NaN
    // threshold would make the auto-compact gate fire on every turn).
    s.auto_compact = form.auto_compact;
    s.compact_threshold = form
        .compact_threshold
        .filter(|t| t.is_finite())
        .unwrap_or(0.80)
        .clamp(0.5, 0.95);
    // Web access (ADR-0017): the checkbox drives the toggle (absent ⇒ off).
    s.web_access = form.web_access;
    // Factored audit (docs/adr/0023): the checkbox drives the toggle (absent ⇒ off).
    s.audit_findings = form.audit_findings;
    // Context-window overrides (dynamic budget, ADR-0014): absent field = keep current value.
    if let Some(n) = form.ollama_ctx_tokens {
        s.ollama_ctx_tokens = clamp_ctx_tokens(n);
    }
    if let Some(n) = form.claude_ctx_tokens {
        s.claude_ctx_tokens = clamp_ctx_tokens(n);
    }
    s.role_tuning = form.role_tuning;
    apply_role_fields(&mut s, &pairs);
    state.llm.set_settings(LlmSettings { ..s });
    tracing::info!(backend = ?state.llm.settings().backend, "llm settings updated");

    Ok(form_view(&state, true))
}
