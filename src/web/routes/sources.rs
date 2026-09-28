//! Sources management page (`GET`/`POST /sources`): the owner's list of named, read-only
//! reference sources, mirroring the MCP module's shape (`web::routes::mcp`) but backed by
//! [`crate::sources::SourceRegistry`]. Every add/update/delete mutation re-renders the swappable
//! `#sources` panel (`_sources_list.html`) so the page never needs a full reload.
//!
//! There is no probe route and no toggle: every GET render stat-probes the registry directly
//! (`SourceRegistry::statuses` is one `read_dir` per source — bounded local disk I/O, nothing
//! like the MCP probe's network round trip), so the status pills are always current, and a
//! source is either registered or removed, never "disabled".
//!
//! **The app never runs docker (ADR-0020).** A mutation here only rewrites the generated compose
//! override; the saved-vs-applied gap (`SourceStatus::NeedsReup`) is surfaced as the banner's
//! "you run: `docker compose up -d`" copy — the owner applies the mount plan, never this process.

use askama::Template as _;
use axum::extract::{Path, State};
use axum::Form;
use serde::Deserialize;
use std::path::PathBuf;

use crate::app::AppState;
use crate::domain::Name;
use crate::sources::{SourceConfig, SourceStatus};
use crate::web::templates::{
    ApplyState, SourceEditRow, SourceRow, SourceRowView, SourcesList, SourcesPage,
};
use crate::web::WebError;

/// Middle-truncate a path for the row display: long host paths would otherwise push the status
/// pill off the card, and the *tail* components are what distinguish sibling paths, so the cut
/// lands in the middle (the full path stays readable in the row's `title` attribute).
/// Char-based, not byte-based — a multibyte path component must never be split mid-codepoint.
pub(crate) fn truncate_path_middle(path: &str, max: usize) -> String {
    let n = path.chars().count();
    if n <= max {
        return path.to_string();
    }
    // One display slot goes to the ellipsis; the tail keeps the larger half (see above).
    let keep = max.saturating_sub(1);
    let head = keep / 2;
    let tail = keep - head;
    let head_str: String = path.chars().take(head).collect();
    let tail_str: String = path.chars().skip(n - tail).collect();
    format!("{head_str}…{tail_str}")
}

/// The display cap for [`truncate_path_middle`] on the Sources rows.
const PATH_DISPLAY_MAX: usize = 64;

/// One status's row rendering: (pill text, pill kind, hint line). The kind is the CSS modifier
/// (`ok`/`stale`/`warn`/`danger`); the hint renders under the row only when non-empty (non-ok).
/// The vocabulary is mode-specific: bare mode reads the host path directly (nothing to re-up),
/// container mode probes the bind mount and distinguishes the ADR-0020 ghost-bind signal
/// (`Mounted { entries: 0 }` — a vanished host dir lists as empty instead of erroring).
fn status_view(bare_mode: bool, status: &SourceStatus) -> (String, &'static str, &'static str) {
    if bare_mode {
        match status {
            SourceStatus::Mounted { .. } => ("readable".to_string(), "ok", ""),
            SourceStatus::Missing => (
                "missing".to_string(),
                "danger",
                "path does not exist or is unreadable",
            ),
            // Unreachable: `status_of` only emits NeedsReup in container mode. Worded as the
            // container copy anyway rather than panicking on a future registry change.
            SourceStatus::NeedsReup => (
                "needs re-up".to_string(),
                "warn",
                "run docker compose up -d",
            ),
        }
    } else {
        match status {
            SourceStatus::Mounted { entries: 0 } => (
                "mounted (empty)".to_string(),
                "stale",
                "mounted but empty — wrong host path, or the dir really is empty",
            ),
            SourceStatus::Mounted { entries } => (format!("mounted ({entries} entries)"), "ok", ""),
            SourceStatus::NeedsReup => (
                "needs re-up".to_string(),
                "warn",
                "run docker compose up -d",
            ),
            SourceStatus::Missing => (
                "missing".to_string(),
                "danger",
                "mountpoint absent after re-up — check the host path",
            ),
        }
    }
}

/// Build one row's view struct from a registry entry + its live status — shared by the full list
/// and `view_source_row` (the edit form's cancel target) so both stay identical.
fn source_row(bare_mode: bool, cfg: SourceConfig, status: SourceStatus) -> SourceRow {
    let (status_text, status_kind, status_hint) = status_view(bare_mode, &status);
    let path_full = cfg.host_path.display().to_string();
    SourceRow {
        name: cfg.name.into(),
        path_display: truncate_path_middle(&path_full, PATH_DISPLAY_MAX),
        path_full,
        status_text,
        status_kind,
        status_hint: status_hint.to_string(),
    }
}

/// The copyable `.env` line that joins the generated override to the owner's compose stack.
/// Derived from the *actual* configured vault dir's final component (the standard layout is
/// `vault/`, but the owner may have pointed `IDEA_VAULT_VAULT_DIR` elsewhere) — built here in
/// Rust, not assembled in the template, so the copy is never a lie about where the file lives.
fn compose_file_line(vault_dir: &std::path::Path) -> String {
    let vault = vault_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("vault");
    format!(
        "COMPOSE_FILE=docker-compose.yml:{vault}/{}",
        crate::sources::OVERRIDE_FILENAME
    )
}

/// Build the current `#sources` panel from the live registry state. Every GET render stat-probes
/// (`statuses`), so a `docker compose up -d` run between two loads flips the pills with no
/// explicit refresh action.
fn list_view(state: &AppState) -> SourcesList {
    let bare_mode = state.config.sources_dir.is_none();
    let statuses = state.sources.statuses();
    let total = statuses.len();
    let pending = statuses
        .iter()
        .filter(|(_, s)| matches!(s, SourceStatus::NeedsReup))
        .count();
    SourcesList {
        sources: statuses
            .into_iter()
            .map(|(cfg, status)| source_row(bare_mode, cfg, status))
            .collect(),
        apply: ApplyState {
            pending,
            total,
            override_path: compose_file_line(&state.config.vault_dir),
            bare_mode,
            // `sources_applied` None in container mode means the override was never layered at
            // all — the one-time COMPOSE_FILE setup instruction is what unblocks the owner.
            show_compose_setup: !bare_mode && state.config.sources_applied.is_none(),
        },
    }
}

/// Look up one source by name or fail with the same 404 shape every `/sources/{name}/*` route
/// uses for a stale panel (the source was removed elsewhere between page-load and this request).
fn find(state: &AppState, name: &str) -> Result<SourceConfig, WebError> {
    state
        .sources
        .get(name)
        .ok_or_else(|| WebError::NotFound(format!("source '{name}'")))
}

/// `GET /sources` — the full page: every registered source plus the add-source form.
pub async fn sources_page(State(state): State<AppState>) -> Result<SourcesPage, WebError> {
    let list_html = list_view(&state)
        .render()
        .map_err(|e| WebError::Internal(format!("template render: {e}")))?;
    Ok(SourcesPage { list_html })
}

#[derive(Debug, Deserialize)]
pub struct AddSourceForm {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub host_path: String,
}

/// `POST /sources/add` — validate + persist a new source via `SourceRegistry::add` (which also
/// regenerates the compose override), then re-render the panel. Rejection (bad name, unsafe
/// path, duplicate) comes back as `400` with `add`'s readable message — same "plain 400, no
/// swap" contract `mcp::add_server` uses.
pub async fn add_source(
    State(state): State<AppState>,
    Form(form): Form<AddSourceForm>,
) -> Result<SourcesList, WebError> {
    let name = Name::try_from(form.name.trim()).map_err(|e| {
        WebError::BadRequest(format!(
            "invalid source name '{}': use lowercase letters, digits and '-' only",
            e.0
        ))
    })?;
    state
        .sources
        .add(SourceConfig {
            name,
            host_path: PathBuf::from(form.host_path.trim()),
        })
        .map_err(WebError::BadRequest)?;
    Ok(list_view(&state))
}

/// `GET /sources/{name}/edit` — swap that row's `#src-row-<name>` into an edit form (host path
/// only; the name is immutable — it is the mount target and the tool routing key, see
/// `SourceRegistry::update_path`).
pub async fn edit_source_form(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<SourceEditRow, WebError> {
    let s = find(&state, &name)?;
    Ok(SourceEditRow {
        name: s.name.into(),
        path: s.host_path.display().to_string(),
    })
}

/// `GET /sources/{name}/view` — the edit form's "cancel": swap `#src-row-<name>` back to its
/// normal view-mode row with no mutation.
pub async fn view_source_row(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<SourceRowView, WebError> {
    let s = find(&state, &name)?;
    let bare_mode = state.config.sources_dir.is_none();
    let status = state
        .sources
        .status(&s.name)
        .ok_or_else(|| WebError::NotFound(format!("source '{name}'")))?;
    Ok(SourceRowView {
        source: source_row(bare_mode, s, status),
    })
}

#[derive(Debug, Deserialize)]
pub struct UpdateSourceForm {
    #[serde(default)]
    pub host_path: String,
}

/// `POST /sources/{name}/update` — apply a host-path edit via `SourceRegistry::update_path`,
/// then re-render the full panel. Existence is checked *before* the update so a stale-panel edit
/// reads as `404` and a same-source bad path reads as `400` — `update_path` can't distinguish
/// the two from its single `Result<(), String>`, so the status-code decision happens here
/// (same split as `mcp::update_server`).
pub async fn update_source(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<UpdateSourceForm>,
) -> Result<SourcesList, WebError> {
    find(&state, &name)?;
    state
        .sources
        .update_path(&name, PathBuf::from(form.host_path.trim()))
        .map_err(WebError::BadRequest)?;
    Ok(list_view(&state))
}

/// `POST /sources/{name}/delete` — remove the source (the override regenerates without it;
/// the host directory itself is untouched), re-render. `404` on an unknown name.
pub async fn delete_source(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<SourcesList, WebError> {
    state.sources.remove(&name).map_err(WebError::NotFound)?;
    Ok(list_view(&state))
}

#[cfg(test)]
mod tests {
    use super::{compose_file_line, status_view, truncate_path_middle};
    use crate::sources::SourceStatus;

    #[test]
    fn truncate_path_middle_passes_short_paths_through() {
        assert_eq!(truncate_path_middle("/srv/docs", 64), "/srv/docs");
        // Exactly at the cap: untouched.
        let exact = "a".repeat(64);
        assert_eq!(truncate_path_middle(&exact, 64), exact);
    }

    #[test]
    fn truncate_path_middle_cuts_the_middle_keeping_head_and_tail() {
        let long = format!("/home/owner/{}/reference/final-notes", "x".repeat(80));
        let out = truncate_path_middle(&long, 64);
        assert_eq!(out.chars().count(), 64);
        assert!(out.starts_with("/home/owner/"), "{out}");
        assert!(out.ends_with("final-notes"), "{out}");
        assert!(out.contains('…'), "{out}");
    }

    #[test]
    fn truncate_path_middle_is_char_safe_on_multibyte_paths() {
        // Must never split a multibyte codepoint — counted in chars, not bytes.
        let long = format!("/home/właściciel/{}", "ż".repeat(100));
        let out = truncate_path_middle(&long, 32);
        assert_eq!(out.chars().count(), 32);
    }

    #[test]
    fn status_vocabulary_matches_the_mode() {
        // Bare mode: readable / missing only.
        let (text, kind, _) = status_view(true, &SourceStatus::Mounted { entries: 3 });
        assert_eq!((text.as_str(), kind), ("readable", "ok"));
        let (text, kind, hint) = status_view(true, &SourceStatus::Missing);
        assert_eq!((text.as_str(), kind), ("missing", "danger"));
        assert!(hint.contains("does not exist"));

        // Container mode: the full four-way vocabulary, incl. the ghost-bind warning.
        let (text, kind, _) = status_view(false, &SourceStatus::Mounted { entries: 2 });
        assert_eq!((text.as_str(), kind), ("mounted (2 entries)", "ok"));
        let (text, kind, hint) = status_view(false, &SourceStatus::Mounted { entries: 0 });
        assert_eq!((text.as_str(), kind), ("mounted (empty)", "stale"));
        assert!(hint.contains("wrong host path"));
        let (text, kind, hint) = status_view(false, &SourceStatus::NeedsReup);
        assert_eq!((text.as_str(), kind), ("needs re-up", "warn"));
        assert!(hint.contains("docker compose up -d"));
        let (text, kind, hint) = status_view(false, &SourceStatus::Missing);
        assert_eq!((text.as_str(), kind), ("missing", "danger"));
        assert!(hint.contains("check the host path"));
    }

    #[test]
    fn compose_file_line_derives_from_the_actual_vault_dir_name() {
        assert_eq!(
            compose_file_line(std::path::Path::new("/srv/app/vault")),
            "COMPOSE_FILE=docker-compose.yml:vault/.docker-compose.sources.yml"
        );
        // A non-standard vault dir name must show up honestly in the copyable line.
        assert_eq!(
            compose_file_line(std::path::Path::new("/data/my-ideas")),
            "COMPOSE_FILE=docker-compose.yml:my-ideas/.docker-compose.sources.yml"
        );
    }
}
