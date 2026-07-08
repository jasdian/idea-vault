//! Knowledge-extraction artifacts route group (docs/adr/0015, docs/09-web-ui.md R18–R20):
//! run an extraction swarm as a background job (R18), view one artifact (R19), and delete one
//! (R20). The per-file management UI mirrors the memory panel.

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use chrono::Utc;

use crate::app::AppState;
use crate::concepts::knowledge;
use crate::domain::{slug as domain_slug, ArtifactKind};
use crate::vault::store;
use crate::web::jobs;
use crate::web::routes::memory::{guard_discussion_state, progress_sink};
use crate::web::routes::reindex_logged;
use crate::web::templates::{
    render_markdown, ArtifactEntry, ArtifactExport, ArtifactPage, ArtifactsPanel, ExportSection,
};
use crate::web::WebError;

/// Form body for R18. The checkbox is present-with-`value="true"` when ticked and omitted from
/// the body otherwise, so `#[serde(default)]` reads unchecked as `false` (the `_settings.html`
/// idiom).
#[derive(Debug, serde::Deserialize)]
pub struct ExtractForm {
    #[serde(default)]
    pub html: bool,
}

/// R18 — `POST /idea/{slug}/extract` — harvest knowledge from the discussion as a background job
/// (ADR-0010): fan out the extraction lenses, persist per-lens artifacts + a synthesis, and
/// optionally (`html=true`) write a standalone `.html` report export. Returns the transcript
/// with the thinking indicator, like skill/swarm.
pub async fn run_extract(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    axum::Form(form): axum::Form<ExtractForm>,
) -> Result<axum::response::Html<String>, WebError> {
    let idea = store::read_idea(&state.config.vault_dir, &slug)?; // 404 if missing
    guard_discussion_state(idea.frontmatter.state)?;

    if !jobs::try_claim(&state.jobs, &slug) {
        return crate::web::routes::ideas::respond_with_transcript(&state, &slug);
    }
    let ts = state.clone();
    let tslug = slug.clone();
    let want_html = form.html;
    let handle = tokio::spawn(async move {
        match run_extract_work(&ts, &tslug, want_html).await {
            Ok(()) => jobs::mark_done(&ts.jobs, &tslug),
            Err(m) => jobs::mark_failed(&ts.jobs, &tslug, m),
        }
    });
    jobs::set_abort(&state.jobs, &slug, handle.abort_handle());
    crate::web::routes::ideas::respond_with_transcript(&state, &slug)
}

async fn run_extract_work(state: &AppState, slug: &str, want_html: bool) -> Result<(), String> {
    let progress = progress_sink(state, slug);
    let outcome = knowledge::extract_knowledge(
        &state.llm,
        &state.ai_semaphore,
        &state.skills,
        &state.config.vault_dir,
        slug,
        knowledge::LENSES.iter().map(|l| l.to_string()).collect(),
        state.llm.context_budget(),
        &progress,
    )
    .await
    .map_err(|e| e.to_string())?;

    // The .html report is a derived export of truth that already landed — a failure (or a
    // cancel racing in) here can cost only the report, never the findings. Degrade, don't fail:
    // the run succeeded the moment the artifacts + turn persisted, so a report error must not
    // mark the job Failed (a misleading error banner under a successful synthesis turn) nor
    // skip the reindex that makes the new artifacts searchable.
    if want_html && !outcome.findings.is_empty() {
        progress("extraction · writing the HTML report");
        if let Err(e) = write_html_report(state, slug, &outcome) {
            tracing::warn!(slug, error = %e, "html report write failed; findings and synthesis are persisted");
        }
    }
    reindex_logged(state);
    Ok(())
}

/// Render the standalone report (`artifact_export.html`) from the just-persisted artifacts and
/// write it as `artifacts/<stamp>-report.html`. Bodies go through the same sanitizing
/// `render_markdown` as every other page; the shell is server-authored.
fn write_html_report(
    state: &AppState,
    slug: &str,
    outcome: &knowledge::KnowledgeOutcome,
) -> Result<(), WebError> {
    use askama::Template as _;
    let vault_dir = &state.config.vault_dir;

    let idea = store::read_idea(vault_dir, slug)?;
    let mut sections = Vec::with_capacity(outcome.findings.len());
    for finding in &outcome.findings {
        let artifact = store::read_artifact(vault_dir, slug, &finding.file_slug)?;
        sections.push(ExportSection {
            title: artifact.frontmatter.title,
            body_html: render_markdown(&artifact.body),
        });
    }

    let html = ArtifactExport {
        idea_title: idea.frontmatter.title,
        generated: Utc::now().format("%Y-%m-%d %H:%M UTC").to_string(),
        model: state.llm.model(),
        summary_html: if outcome.synthesis.is_empty() {
            String::new()
        } else {
            render_markdown(&outcome.synthesis)
        },
        sections,
    }
    .render()
    .map_err(|e| WebError::Internal(format!("template render: {e}")))?;

    let taken = |c: &str| store::artifact_exists(vault_dir, slug, c).unwrap_or(false);
    let stem = domain_slug::disambiguate(&format!("{}-report", outcome.run_stamp), taken);
    store::write_artifact_html(vault_dir, slug, &stem, &html)?;
    Ok(())
}

/// Split an `{name}` path segment into (stem, extension), admitting only the two artifact
/// extensions and the canonical slug charset for the stem — defense in depth against traversal
/// before any store call (which re-validates).
fn split_artifact_name(name: &str) -> Option<(&str, store::ArtifactExt)> {
    let (stem, ext) = name.rsplit_once('.')?;
    let ext = match ext {
        "md" => store::ArtifactExt::Md,
        "html" => store::ArtifactExt::Html,
        _ => return None,
    };
    if !domain_slug::is_valid(stem) {
        return None;
    }
    Some((stem, ext))
}

/// The one-line provenance shown for a `.md` artifact in the panel and on its page.
fn artifact_meta(fm: &crate::domain::ArtifactFrontmatter) -> String {
    let when = fm.created.format("%Y-%m-%d");
    match (&fm.kind, &fm.lens) {
        (ArtifactKind::Synthesis, _) => format!("synthesis · {when}"),
        (ArtifactKind::Finding, Some(lens)) => {
            format!("finding · {} · {when}", knowledge::lens_short(lens))
        }
        (ArtifactKind::Finding, None) => format!("finding · {when}"),
    }
}

/// R19 — `GET /idea/{slug}/artifact/{name}` — view one artifact: a `.md` renders as a full page
/// through the sanitizing markdown pipeline; a `.html` report export is served as-is (its body
/// was sanitized at build time and the shell is server-authored).
pub async fn view_artifact(
    State(state): State<AppState>,
    Path((slug, name)): Path<(String, String)>,
) -> Result<Response, WebError> {
    let vault_dir = &state.config.vault_dir;
    let Some((stem, ext)) = split_artifact_name(&name) else {
        return Err(WebError::NotFound(format!("artifact: {name}")));
    };
    let idea = store::read_idea(vault_dir, &slug)?; // 404 if missing

    match ext {
        store::ArtifactExt::Md => {
            let artifact = store::read_artifact(vault_dir, &slug, stem)?;
            Ok(ArtifactPage {
                title: artifact.frontmatter.title.clone(),
                idea_slug: slug,
                idea_title: idea.frontmatter.title,
                file_name: name,
                meta: artifact_meta(&artifact.frontmatter),
                content_html: render_markdown(&artifact.body),
            }
            .into_response())
        }
        store::ArtifactExt::Html => {
            let raw = store::read_artifact_html(vault_dir, &slug, stem)?;
            // Defense in depth: the report body was ammonia-sanitized at build time, but the
            // vault is an owner-editable directory — a hand-edited (or tampered) .html must not
            // gain same-origin script execution. The CSP admits the export's inline styles and
            // nothing else (no scripts, no network, no frames).
            Ok((
                [
                    (
                        "Content-Security-Policy",
                        "default-src 'none'; style-src 'unsafe-inline'; img-src data:",
                    ),
                    ("X-Content-Type-Options", "nosniff"),
                ],
                axum::response::Html(raw),
            )
                .into_response())
        }
    }
}

/// R20 — `POST /idea/{slug}/artifact/{name}/delete` — delete one artifact file (deliberate,
/// confirm-gated cleanup) and return the re-rendered panel; `.md` deletions also reindex (they
/// have FTS rows).
pub async fn delete_artifact(
    State(state): State<AppState>,
    Path((slug, name)): Path<(String, String)>,
) -> Result<axum::response::Html<String>, WebError> {
    let vault_dir = &state.config.vault_dir;
    let Some((stem, ext)) = split_artifact_name(&name) else {
        return Err(WebError::NotFound(format!("artifact: {name}")));
    };
    store::read_idea(vault_dir, &slug)?; // 404 if the idea is gone
    if !store::delete_artifact(vault_dir, &slug, stem, ext)? {
        return Err(WebError::NotFound(format!("artifact: {name}")));
    }
    reindex_logged(&state);
    Ok(axum::response::Html(render_artifacts_panel(
        vault_dir, &slug, false,
    )?))
}

/// Render the artifacts panel (`_artifacts.html`): `.md` rows carry their parsed title +
/// provenance, `.html` rows their stem. Shared by the idea page and the delete route (which
/// swaps `#artifacts`). Order follows `list_artifact_files` — (stem, ext), i.e. chronological
/// by run stamp.
pub(crate) fn render_artifacts_panel(
    vault_dir: &std::path::Path,
    idea_slug: &str,
    oob: bool,
) -> Result<String, WebError> {
    use askama::Template as _;

    // Degrade, don't 500: one hand-edited unparsable .md must not take the panel (and with it
    // the idea page) down — it lists under its stem and stays deletable.
    let artifacts = store::read_artifacts(vault_dir, idea_slug).unwrap_or_else(|e| {
        tracing::warn!(idea_slug, error = %e, "unparsable artifacts; listing by file name only");
        Vec::new()
    });
    let entries = store::list_artifact_files(vault_dir, idea_slug)?
        .into_iter()
        .map(|file| {
            let file_name = format!("{}.{}", file.slug, file.ext.as_str());
            match file.ext {
                store::ArtifactExt::Md => {
                    let parsed = artifacts
                        .iter()
                        .find(|a| a.frontmatter.slug == file.slug)
                        .map(|a| (a.frontmatter.title.clone(), artifact_meta(&a.frontmatter)));
                    // An .md that failed to parse still lists (deletable), under its stem.
                    let (title, meta) =
                        parsed.unwrap_or_else(|| (file.slug.clone(), "unparsable".to_string()));
                    ArtifactEntry {
                        file_name,
                        title,
                        meta,
                        is_html: false,
                    }
                }
                store::ArtifactExt::Html => ArtifactEntry {
                    file_name,
                    title: file.slug.clone(),
                    meta: "html report".to_string(),
                    is_html: true,
                },
            }
        })
        .collect();

    ArtifactsPanel {
        idea_slug: idea_slug.to_string(),
        entries,
        oob,
    }
    .render()
    .map_err(|e| WebError::Internal(format!("template render: {e}")))
}
