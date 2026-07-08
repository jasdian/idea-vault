//! Askama template structs backing `templates/*.html` (docs/09-web-ui.md, template hierarchy).
//!
//! One struct per rendered template so all eight compile. `base.html` is only ever extended, so it
//! has no struct. `#[derive(askama::Template, askama_web::WebTemplate)]` yields `IntoResponse`.

use askama::Template;
use askama_web::WebTemplate;

use crate::domain::memory::MemoryIndexEntry;
use crate::index::queries::{IdeaSummary, SearchHit};

/// Render markdown to sanitized HTML (docs/09-web-ui.md: "the browser only receives HTML").
/// Sanitization is unconditional — idea bodies, memory facts, and conversation turns all carry
/// AI- or owner-authored text, and none of it may smuggle script into the page.
pub fn render_markdown(markdown: &str) -> String {
    let mut options = pulldown_cmark::Options::empty();
    options.insert(pulldown_cmark::Options::ENABLE_TABLES);
    options.insert(pulldown_cmark::Options::ENABLE_STRIKETHROUGH);
    let parser = pulldown_cmark::Parser::new_ext(markdown, options);
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, parser);
    ammonia::clean(&html)
}

/// Full page: idea list + search box + new-idea form (R1, `templates/list.html`).
#[derive(Template, WebTemplate)]
#[template(path = "list.html")]
pub struct ListPage {
    pub ideas: Vec<IdeaSummary>,
}

/// Full page: one idea (body, conversation, memory) (R2, `templates/idea.html`).
#[derive(Template, WebTemplate)]
#[template(path = "idea.html")]
pub struct IdeaPage {
    pub title: String,
    pub slug: String,
    pub state: String,
    pub body_html: String,
    /// The always-on memory panel, pre-rendered (`_memory.html`) so a fact deletion can swap it.
    pub memory_html: String,
    /// The state-dependent lower panel, pre-rendered (`_discussion.html` or `_stored.html`) so
    /// the partials stay the single source for both full-page and HTMX-swap rendering.
    pub panel_html: String,
    /// The artifacts panel, pre-rendered (`_artifacts.html`) so a file deletion can swap it.
    pub artifacts_html: String,
}

/// Partial: the memory panel (`templates/_memory.html`) — the MEMORY.md index with a per-fact
/// delete control. Re-rendered on its own after a fact deletion (swapped into `#memory`).
#[derive(Template, WebTemplate)]
#[template(path = "_memory.html")]
pub struct MemoryPanel {
    pub idea_slug: String,
    pub entries: Vec<MemoryIndexEntry>,
}

/// The "btw" history view (`templates/history.html`): the whole thread on its own page + Fork.
#[derive(Template, WebTemplate)]
#[template(path = "history.html")]
pub struct HistoryPage {
    pub title: String,
    pub slug: String,
    pub transcript_html: String,
}

/// The settings page shell (`templates/settings.html`); the form is pre-rendered so a save can
/// swap just the form.
#[derive(Template, WebTemplate)]
#[template(path = "settings.html")]
pub struct SettingsPage {
    pub form_html: String,
}

/// Partial: the live LLM controls form (`templates/_settings.html`), returned on save with
/// `saved = true` for the confirmation.
#[derive(Template, WebTemplate)]
#[template(path = "_settings.html")]
pub struct SettingsForm {
    pub is_ollama: bool,
    pub ollama_model: String,
    pub temperature: String,
    pub claude_model: String,
    pub effort: String,
    /// Auto-compact toggle + trigger fraction (docs/adr/0012).
    pub auto_compact: bool,
    pub compact_threshold: String,
    /// Per-backend context-window overrides in tokens ("0" = auto, derived from the model).
    pub ollama_ctx_tokens: String,
    pub claude_ctx_tokens: String,
    /// The window the active backend resolves to right now (tokens) — the "effective" hint.
    pub effective_ctx: String,
    pub saved: bool,
}

/// Partial: a single idea row in the list (R3, `templates/_idea_row.html`).
#[derive(Template, WebTemplate)]
#[template(path = "_idea_row.html")]
pub struct IdeaRow {
    pub idea: IdeaSummary,
}

/// Partial: one conversation turn (R6/R7/R9, `templates/_turn.html`). Carries a display label
/// (`you` / `foil · premortem`), whether it's the owner's turn, and its slug + 0-based transcript
/// index for the per-turn remove control.
#[derive(Template, WebTemplate)]
#[template(path = "_turn.html")]
pub struct Turn {
    pub label: String,
    pub is_user: bool,
    pub content_html: String,
    pub slug: String,
    pub index: usize,
}

/// Partial: the discussion pane (compose box + SSE target) (R5, `templates/_discussion.html`).
#[derive(Template, WebTemplate)]
#[template(path = "_discussion.html")]
pub struct Discussion {
    pub slug: String,
    pub ai_available: bool,
    /// The D20 per-state remedy shown in the banner when AI is unavailable. Backend-aware
    /// (`routes::ideas::availability_hint`): `ollama pull <model>` for ModelMissing; for
    /// Unreachable, `ollama serve` under the Ollama backend or a `claude`-CLI hint under claude-code.
    pub unavailable_hint: String,
    /// The full `#transcript` inner HTML (turns + any job indicator/error + usage meter),
    /// produced by `routes::ideas::transcript_inner` — the single source for page + swap + poll.
    pub transcript_html: String,
    /// The `#idea-actions` block (`_actions.html`), pre-rendered so the same partial serves the
    /// full page and the out-of-band swap transcript responses carry (empty shell in Draft).
    pub actions_html: String,
}

/// Partial: the state-dependent action block (moves/swarm/compact/store) (`templates/_actions.html`).
/// The `#idea-actions` container always renders — empty when `can_store` is false — so the
/// out-of-band swap carried by transcript responses has a target even on a Draft page. With
/// `oob = true` the root carries `hx-swap-oob="true"` and htmx replaces the in-page container
/// instead of swapping it into `#transcript`.
#[derive(Template, WebTemplate)]
#[template(path = "_actions.html")]
pub struct Actions {
    pub slug: String,
    /// Whether Store is a legal D9 transition from the idea's current state
    /// (InDiscussion/Reopened yes; Draft/Stored no — the UI must not offer a guaranteed 400).
    pub can_store: bool,
    /// The registry's skill names — the "menu of moves" (docs/06-concepts/skills.md).
    pub skill_names: Vec<String>,
    pub oob: bool,
}

/// One row of the artifacts panel: a file under `vault/<slug>/artifacts/` (docs/adr/0015).
pub struct ArtifactEntry {
    /// Full file name including extension (`<stem>.md` / `<stem>.html`) — the view/delete key.
    pub file_name: String,
    /// The artifact title for `.md` truth files; the file stem for `.html` exports.
    pub title: String,
    /// One-line provenance ("finding · key decisions" / "synthesis" / "html report").
    pub meta: String,
    pub is_html: bool,
}

/// Partial: the artifacts panel (`templates/_artifacts.html`) — every extraction artifact with
/// view + per-file delete controls. Re-rendered on its own after a deletion (swaps `#artifacts`).
#[derive(Template, WebTemplate)]
#[template(path = "_artifacts.html")]
pub struct ArtifactsPanel {
    pub idea_slug: String,
    pub entries: Vec<ArtifactEntry>,
    /// With `oob = true` the root carries `hx-swap-oob="true"` — transcript responses append
    /// this fragment so a finished extraction surfaces its files without a reload (the panel
    /// sits outside `#transcript`, like the state badge and actions block).
    pub oob: bool,
}

/// Full page: one rendered `.md` artifact (R19, `templates/artifact.html`).
#[derive(Template, WebTemplate)]
#[template(path = "artifact.html")]
pub struct ArtifactPage {
    pub title: String,
    pub idea_slug: String,
    pub idea_title: String,
    pub file_name: String,
    pub meta: String,
    pub content_html: String,
}

/// One findings section of the standalone HTML report export.
pub struct ExportSection {
    pub title: String,
    pub body_html: String,
}

/// The standalone `.html` report export (`templates/artifact_export.html`) — written to disk as
/// a derived artifact, NOT served as a response, so `Template` only (no `WebTemplate`). Fully
/// self-contained: own doctype, inline styles, no `/static` references.
#[derive(Template)]
#[template(path = "artifact_export.html")]
pub struct ArtifactExport {
    pub idea_title: String,
    pub generated: String,
    pub model: String,
    /// Rendered synthesis, empty when the synthesizer produced nothing (findings still ship).
    pub summary_html: String,
    pub sections: Vec<ExportSection>,
}

/// Partial: stored view (consolidated body + memory) (R4, `templates/_stored.html`).
#[derive(Template, WebTemplate)]
#[template(path = "_stored.html")]
pub struct Stored {
    pub slug: String,
    pub body_html: String,
}

/// Partial: full-text search results (R8, `templates/_search_results.html`).
#[derive(Template, WebTemplate)]
#[template(path = "_search_results.html")]
pub struct SearchResults {
    pub hits: Vec<SearchHit>,
}
