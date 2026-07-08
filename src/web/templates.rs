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

/// Partial: the idea-page title block (`templates/_idea_title.html`) — the `h1` plus its inline
/// rename disclosure. `{% include %}`-d by `IdeaPage` (sharing its `title`/`slug` scope, same
/// trick as `McpRow`'s doc comment) and returned standalone by `POST /idea/{slug}/rename` so the
/// swap re-renders with the disclosure back in its closed state — no separate "cancel" route
/// needed (docs/09-web-ui.md route map: rename does not appear as its own template group, it
/// reuses this one).
#[derive(Template, WebTemplate)]
#[template(path = "_idea_title.html")]
pub struct IdeaTitle {
    pub title: String,
    pub slug: String,
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
    /// Web access toggle (ADR-0017): the foil may search the web / fetch pages on either backend.
    pub web_access: bool,
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
    /// A job is currently running for this idea. Store is a commitment action, so its button
    /// renders `disabled` while busy (a click would only bounce off `try_claim` anyway); the OOB
    /// actions refresh re-enables it once the job finishes or is cancelled.
    pub busy: bool,
    /// The built-in deterministic workflows (D19) — one chip each, next to swarm.
    pub workflows: Vec<WorkflowChip>,
    pub oob: bool,
}

/// One workflow button: name (the route segment) + description (the hover title).
pub struct WorkflowChip {
    pub name: String,
    pub description: String,
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

/// Partial: the dormant-idea panel (R4, `templates/_stored.html`) — label + reopen only. The
/// consolidated writeup is NOT part of it: that text IS the idea body, rendered once in the
/// page's top `.statement` (and refreshed out-of-band when a store job lands), so rendering it
/// here too duplicated the whole writeup on every stored idea page.
#[derive(Template, WebTemplate)]
#[template(path = "_stored.html")]
pub struct Stored {
    pub slug: String,
}

/// The MCP servers page shell (`templates/mcp.html`); the list is pre-rendered so a mutation can
/// swap just the `#mcp` panel, same split as `SettingsPage`/`SettingsForm`.
#[derive(Template, WebTemplate)]
#[template(path = "mcp.html")]
pub struct McpPage {
    pub list_html: String,
}

/// Partial: the swappable MCP panel (`templates/_mcp_list.html`) — the configured-server list plus
/// the add-server form. Returned by `GET /mcp` (embedded) and by every mutating `/mcp/*` route
/// (add/toggle/delete) so the panel reflects the registry without a full reload.
#[derive(Template, WebTemplate)]
#[template(path = "_mcp_list.html")]
pub struct McpList {
    pub servers: Vec<McpServerRow>,
}

/// One configured server row. `has_token` only ever renders as "token set" / "no token" — the
/// bearer token itself must never reach the page (task requirement: never echo it back).
/// `status_html` is the pre-rendered idle placeholder (`McpStatus`) so the row always has a
/// `#mcp-status-<name>` target for `probe` to swap, even before the owner ever probes it.
pub struct McpServerRow {
    pub name: String,
    pub url: String,
    pub has_token: bool,
    pub enabled: bool,
    pub status_html: String,
}

/// Partial: a single server's view-mode `<li>` (`templates/_mcp_row.html`). `{% include %}`-d by
/// `_mcp_list.html` for every row in the loop (Askama includes share the parent's scope, so the
/// loop's `server` binding is visible to the included template) — the *same* field name (`server`)
/// doubles as this struct's top-level field, which is what lets `GET /mcp/{name}/edit`'s cancel
/// action (`GET /mcp/{name}/view`) render exactly one row standalone with no template duplication.
#[derive(Template, WebTemplate)]
#[template(path = "_mcp_row.html")]
pub struct McpRow {
    pub server: McpServerRow,
}

/// Partial: one server's edit-mode `<li>` (`templates/_mcp_edit_row.html`), swapped in by
/// `GET /mcp/{name}/edit` over the same `#mcp-row-<name>` id the view row uses, and posted by
/// `POST /mcp/{name}/update`. `url` is the current value so the form starts populated; there is
/// deliberately no `token` field here — the bearer token is write-only (see `McpServerRow` doc),
/// so the form only ever shows *whether* one is set (`has_token`, used for the placeholder text
/// and to gray out "clear token" when there is nothing to clear).
#[derive(Template, WebTemplate)]
#[template(path = "_mcp_edit_row.html")]
pub struct McpEditRow {
    pub name: String,
    pub url: String,
    pub has_token: bool,
}

/// Partial: one row's probe-status slot (`templates/_mcp_status.html`), swapped in by
/// `POST /mcp/{name}/probe` (`hx-target="#mcp-status-<name>" hx-swap="outerHTML"`) and also used to
/// pre-render every row's idle placeholder on `GET /mcp`. `ok`/`errored` pick the chip color;
/// both false is the neutral "not probed yet" state.
#[derive(Template, WebTemplate)]
#[template(path = "_mcp_status.html")]
pub struct McpStatus {
    pub name: String,
    pub text: String,
    pub ok: bool,
    pub errored: bool,
}

/// Partial: full-text search results (R8, `templates/_search_results.html`).
#[derive(Template, WebTemplate)]
#[template(path = "_search_results.html")]
pub struct SearchResults {
    pub hits: Vec<SearchHit>,
}
