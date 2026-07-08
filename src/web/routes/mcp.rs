//! MCP server management page (`GET`/`POST /mcp`): the owner's list of Model Context Protocol
//! endpoints, mirroring the Settings module's shape (`web::routes::settings`) but backed by
//! [`crate::mcp::McpRegistry`] instead of the live `LlmSettings`. Every add/toggle/delete mutation
//! re-renders the swappable `#mcp` panel (`_mcp_list.html`) so the page never needs a full reload;
//! `probe` is the one route here that touches the network, and it hx-targets only the probed row's
//! status slot (`_mcp_status.html`) rather than the whole panel.
//!
//! `probe` runs the `ai::mcp` handshake + `tools/list` directly in the handler, not through
//! `web::jobs` — that background-job machinery exists to keep a slow *model* call from blocking a
//! request past the browser navigating away, but an MCP probe is a single bounded HTTP round trip
//! (`ai::mcp`'s own 3s connect / 15s request timeouts already cap it), so awaiting it inline is
//! both simpler and fast enough to feel synchronous.
//!
//! Bearer tokens are write-only from the browser's perspective: the add form accepts one, but no
//! response ever echoes a token back — rows render "token set" / "no token" only, and nothing here
//! logs a token value (only the server `name`, which is never secret).

use askama::Template as _;
use axum::extract::{Path, State};
use axum::Form;
use serde::Deserialize;

use crate::ai::mcp::McpClient;
use crate::app::AppState;
use crate::mcp::{McpServerConfig, TokenChange};
use crate::web::templates::{McpEditRow, McpList, McpPage, McpRow, McpServerRow, McpStatus};
use crate::web::WebError;

/// The neutral "not probed yet" placeholder for one row — rendered fresh on every list view since
/// no probe result is cached across requests (a stale "ok" would be misleading after an edit).
/// Bound a probe failure for the status chip: `ai::mcp` errors can embed whole response bodies
/// or reqwest error chains, and an unbounded string blows up the row layout (the full text is
/// still in the warn log).
fn truncate_status(message: String) -> String {
    const MAX: usize = 160;
    match message.char_indices().nth(MAX) {
        Some((idx, _)) => format!("{}…", &message[..idx]),
        None => message,
    }
}

fn idle_status(name: &str) -> McpStatus {
    McpStatus {
        name: name.to_string(),
        text: "not probed".to_string(),
        ok: false,
        errored: false,
    }
}

/// Build one row's view struct from a registry entry — shared by the full list, `view_server_row`
/// (the edit form's cancel target), and `update_server`'s re-render so all three stay identical.
fn server_row(s: McpServerConfig) -> McpServerRow {
    McpServerRow {
        status_html: render(idle_status(&s.name)),
        has_token: s.bearer_token.is_some(),
        name: s.name,
        url: s.url,
        enabled: s.enabled,
    }
}

/// Build the current `#mcp` panel from the live registry state.
fn list_view(state: &AppState) -> McpList {
    McpList {
        servers: state.mcp.list().into_iter().map(server_row).collect(),
    }
}

/// Look up one server by name or fail with the same 404 shape every `/mcp/{name}/*` route uses
/// for a stale panel (the server was deleted elsewhere between page-load and this request).
fn find(state: &AppState, name: &str) -> Result<McpServerConfig, WebError> {
    state
        .mcp
        .list()
        .into_iter()
        .find(|s| s.name == name)
        .ok_or_else(|| WebError::NotFound(format!("mcp server '{name}'")))
}

/// Render any Askama template to a `String`, mapping a (practically-never-hit) render failure to
/// `WebError::Internal` — the same discipline `settings::form_view`'s callers use.
fn render(t: impl askama::Template) -> String {
    t.render().unwrap_or_default()
}

/// `GET /mcp` — the full page: every configured server plus the add-server form.
pub async fn mcp_page(State(state): State<AppState>) -> Result<McpPage, WebError> {
    let list_html = list_view(&state)
        .render()
        .map_err(|e| WebError::Internal(format!("template render: {e}")))?;
    Ok(McpPage { list_html })
}

#[derive(Debug, Deserialize)]
pub struct AddServerForm {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub url: String,
    /// Absent or blank ⇒ no auth header on this server's requests (`McpRegistry`/`McpClient`
    /// contract) — never a placeholder token.
    #[serde(default)]
    pub bearer_token: String,
}

/// `POST /mcp/add` — validate + persist a new server via `McpRegistry::add`, then re-render the
/// panel. Rejection (bad name/url, duplicate name) comes back as `400` with `add`'s readable
/// message — same "plain 400, no swap" contract `settings::update_settings` uses for an unknown
/// backend, rather than an inline re-render that htmx would silently refuse to swap on a non-2xx.
pub async fn add_server(
    State(state): State<AppState>,
    Form(form): Form<AddServerForm>,
) -> Result<McpList, WebError> {
    let bearer_token = {
        let trimmed = form.bearer_token.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    };
    state
        .mcp
        .add(McpServerConfig {
            name: form.name.trim().to_string(),
            url: form.url.trim().to_string(),
            bearer_token,
            enabled: true,
        })
        .map_err(WebError::BadRequest)?;
    Ok(list_view(&state))
}

/// `POST /mcp/{name}/toggle` — flip `enabled`, persist, re-render. `404` on an unknown name (a
/// stale panel after a delete elsewhere), matching the idea-route convention for a missing target.
pub async fn toggle_server(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<McpList, WebError> {
    let current = find(&state, &name)?;
    state
        .mcp
        .set_enabled(&name, !current.enabled)
        .map_err(WebError::NotFound)?;
    Ok(list_view(&state))
}

/// `POST /mcp/{name}/delete` — remove (URL + token gone from disk too), re-render. `404` on an
/// unknown name, same rationale as `toggle_server`.
pub async fn delete_server(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<McpList, WebError> {
    state.mcp.remove(&name).map_err(WebError::NotFound)?;
    Ok(list_view(&state))
}

/// `GET /mcp/{name}/edit` — swap that row's `#mcp-row-<name>` into an edit form (url + bearer
/// token; name is immutable, see `McpEditRow` doc). Never reads a token value into the response —
/// only `has_token` (used for the placeholder text and to gray out "clear token" when there's
/// nothing to clear).
pub async fn edit_server_form(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<McpEditRow, WebError> {
    let s = find(&state, &name)?;
    Ok(McpEditRow {
        name: s.name,
        url: s.url,
        has_token: s.bearer_token.is_some(),
    })
}

/// `GET /mcp/{name}/view` — the edit form's "cancel": swap `#mcp-row-<name>` back to its normal
/// view-mode row with no mutation. Also usable standalone as "give me row N" if a future caller
/// needs it, though today only the edit form's cancel button does.
pub async fn view_server_row(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<McpRow, WebError> {
    let s = find(&state, &name)?;
    Ok(McpRow {
        server: server_row(s),
    })
}

#[derive(Debug, Deserialize)]
pub struct UpdateServerForm {
    #[serde(default)]
    pub url: String,
    /// Blank ⇒ keep the current token; see `TokenChange` for the full three-way contract.
    #[serde(default)]
    pub bearer_token: String,
    /// Checkbox: present (any value) when ticked, absent when not — axum's `Form` extractor
    /// leaves an unticked HTML checkbox out of the body entirely, so `Option` is the correct shape
    /// (not `bool`, which would fail to deserialize on the absent case).
    #[serde(default)]
    pub clear_token: Option<String>,
}

/// `POST /mcp/{name}/update` — apply a url/token edit via `McpRegistry::update`, then re-render
/// the full panel (`#mcp`), matching the swap target every other mutating `/mcp/*` route uses.
/// Existence is checked *before* calling `update` so a stale-panel edit reads as `404` and a
/// same-server bad url reads as `400` — `update` itself can't distinguish the two from its single
/// `Result<(), String>`, so the status-code decision has to happen here.
pub async fn update_server(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<UpdateServerForm>,
) -> Result<McpList, WebError> {
    find(&state, &name)?;
    let token = form.bearer_token.trim();
    let token_change = if form.clear_token.is_some() {
        TokenChange::Clear
    } else if token.is_empty() {
        TokenChange::Keep
    } else {
        TokenChange::Set(token.to_string())
    };
    state
        .mcp
        .update(&name, form.url.trim().to_string(), token_change)
        .map_err(WebError::BadRequest)?;
    Ok(list_view(&state))
}

/// `POST /mcp/{name}/probe` — connect + `tools/list` against the live server and render the
/// result into that row's status slot. A transport/protocol failure is not a `WebError` — every
/// `ai::mcp` failure mode is already a readable `Err(String)` (D20-style degrade discipline), so it
/// renders as the row's error chip instead of a 5xx; only an unknown server `name` is a real 404.
pub async fn probe_server(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<McpStatus, WebError> {
    let server = find(&state, &name)?;

    let outcome = probe(&server).await;
    match &outcome {
        Ok((n, def_bytes)) => {
            // Feed the usage meter's "(+N KB tools)" term ahead of the first turn — the probe is
            // the earliest moment the schemas' size is known (ADR-0017 honest-meter rule).
            state.mcp.note_tools_bytes(&name, *def_bytes);
            tracing::info!(server = %name, tools = n, "mcp probe ok");
        }
        Err(e) => tracing::warn!(server = %name, error = %e, "mcp probe failed"),
    }
    Ok(match outcome {
        Ok((n, _)) => McpStatus {
            name,
            text: format!("{n} tools · ok"),
            ok: true,
            errored: false,
        },
        Err(message) => McpStatus {
            name,
            text: truncate_status(message),
            ok: false,
            errored: true,
        },
    })
}

/// The bounded network call: `initialize` then `tools/list`, returning the tool count plus the
/// serialized size of the mangled definitions (what a turn would actually splice into context —
/// the meter's unit). Split out from the handler so the wiring above stays free of the `?`-chain.
async fn probe(server: &McpServerConfig) -> Result<(usize, usize), String> {
    let client = McpClient::new(server.url.clone(), server.bearer_token.clone())?;
    let mut session = client.connect().await?;
    let tools = session.list_tools().await?;
    let one = [(server.name.clone(), tools.clone())];
    let def_bytes = crate::ai::backend::merged_tool_definitions(None, &one)
        .to_string()
        .len();
    Ok((tools.len(), def_bytes))
}
