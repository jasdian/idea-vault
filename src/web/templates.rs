//! Askama template structs backing `templates/*.html` (docs/09-web-ui.md, template hierarchy).
//!
//! One struct per rendered template so all eight compile. `base.html` is only ever extended, so it
//! has no struct. `#[derive(askama::Template, askama_web::WebTemplate)]` yields `IntoResponse`.

use askama::Template;
use askama_web::WebTemplate;

use crate::index::queries::{IdeaSummary, SearchHit};

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
