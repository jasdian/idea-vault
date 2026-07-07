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
    /// The always-on memory panel: MEMORY.md index entries (empty until first Store).
    pub memory_entries: Vec<MemoryIndexEntry>,
    /// The state-dependent lower panel, pre-rendered (`_discussion.html` or `_stored.html`) so
    /// the partials stay the single source for both full-page and HTMX-swap rendering.
    pub panel_html: String,
}

/// Partial: a single idea row in the list (R3, `templates/_idea_row.html`).
#[derive(Template, WebTemplate)]
#[template(path = "_idea_row.html")]
pub struct IdeaRow {
    pub idea: IdeaSummary,
}

/// Partial: one conversation turn (R6/R7/R9, `templates/_turn.html`).
#[derive(Template, WebTemplate)]
#[template(path = "_turn.html")]
pub struct Turn {
    pub role: String,
    pub content_html: String,
}

/// Partial: the discussion pane (compose box + SSE target) (R5, `templates/_discussion.html`).
#[derive(Template, WebTemplate)]
#[template(path = "_discussion.html")]
pub struct Discussion {
    pub slug: String,
    pub ai_available: bool,
    /// Whether Store is a legal D9 transition from the idea's current state
    /// (InDiscussion/Reopened yes; Draft no — the UI must not offer a guaranteed 400).
    pub can_store: bool,
    /// The D20 per-state remedy shown in the banner when AI is unavailable
    /// (`ollama serve` for Unreachable, `ollama pull <model>` for ModelMissing).
    pub unavailable_hint: String,
    /// Existing transcript turns, each pre-rendered via [`Turn`] (single source: `_turn.html`).
    pub turns_html: Vec<String>,
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
