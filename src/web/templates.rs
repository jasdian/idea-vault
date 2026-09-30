//! Askama template structs backing `templates/*.html` (docs/09-web-ui.md, template hierarchy).
//!
//! One struct per rendered template so all eight compile. `base.html` is only ever extended, so it
//! has no struct. `#[derive(askama::Template, askama_web::WebTemplate)]` yields `IntoResponse`.

use askama::Template;
use askama_web::WebTemplate;

use crate::domain::memory::MemoryIndexEntry;
use crate::index::queries::IdeaSummary;

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
    /// `Some(tag)` when the list is filtered to one tag (`GET /?tag=x`) — renders the
    /// "filtered by #x · clear" line.
    pub filter_tag: Option<String>,
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
    /// The tag row (`_idea_tags.html`), pre-rendered so the tags editor can swap just it.
    pub tags_html: String,
    /// The attached-sources row (`_idea_sources.html`), pre-rendered so the attach editor can
    /// swap just it — same split as `tags_html`.
    pub sources_html: String,
    /// The related-ideas panel (`_related.html`), pre-rendered like the other panels.
    pub related_html: String,
    /// The newest run journal's id, for the "last run" link to R50 (ADR-0037).
    pub last_run: Option<String>,
}

/// A tag this idea carries that looks like drift of another tag in the vault.
pub struct TagDriftNote {
    pub own_tag: String,
    pub other_tag: String,
    pub carriers: String,
}

/// Partial: the related-ideas panel (`templates/_related.html`).
#[derive(Template)]
#[template(path = "_related.html")]
pub struct RelatedPanel {
    pub entries: Vec<crate::memory::related::RelatedEntryView>,
    pub drift: Vec<TagDriftNote>,
    /// The index lookup failed or its lock was poisoned: say so instead of claiming there are none.
    pub unavailable: bool,
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

/// Partial: the idea's tag chips + inline editor (`templates/_idea_tags.html`), swapped whole
/// by `POST /idea/{slug}/tags`.
#[derive(Template, WebTemplate)]
#[template(path = "_idea_tags.html")]
pub struct IdeaTags {
    pub slug: String,
    pub tags: Vec<String>,
    /// The comma-joined form-input prefill ("trading, tooling") — precomputed because Askama
    /// templates shouldn't be doing string joins.
    pub tags_joined: String,
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

/// The run inspector (`templates/run.html`, R50, ADR-0037): one run journal, call by call.
#[derive(Template, WebTemplate)]
#[template(path = "run.html")]
pub struct RunPage {
    pub slug: String,
    pub idea_title: String,
    pub run_id: String,
    pub kind: String,
    pub build: String,
    /// `done`, `failed: <message>`, `cancelled`, `panicked`, or unfinished.
    pub outcome: String,
    pub calls: Vec<RunCallView>,
    /// Lines that did not parse (a torn tail after a crash).
    pub unreadable: usize,
}

/// One model call of a run, as R50 shows it.
pub struct RunCallView {
    pub seq: u64,
    pub role: String,
    pub backend: String,
    pub model: String,
    /// `<contract> · clean|repaired|retried|off-contract: <why>`, or `not checked`.
    pub contract: String,
    pub off_contract: bool,
    pub prompt_tokens: String,
    pub output_tokens: String,
    pub api_calls: u64,
    pub stop_reason: String,
    /// `output truncated`, `input truncated`, both, or empty.
    pub truncation: String,
    pub ms: u64,
    pub response: String,
    pub tools: Vec<String>,
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
    /// Factored-audit toggle (docs/adr/0023): swarms/workflows judge findings before synthesis.
    pub audit_findings: bool,
    /// Per-backend context-window overrides in tokens ("0" = auto, derived from the model).
    pub ollama_ctx_tokens: String,
    pub claude_ctx_tokens: String,
    /// The window the active backend resolves to right now (tokens) — the "effective" hint.
    pub effective_ctx: String,
    /// Per-role call profiles toggle (docs/adr/0026) and one row per agent role.
    pub role_tuning: bool,
    pub roles: Vec<RoleRow>,
    pub saved: bool,
}

/// One agent role's row in the settings role table (docs/adr/0026); blank model/effort inherit.
pub struct RoleRow {
    pub name: &'static str,
    pub temperature: String,
    pub claude_model: String,
    pub effort: String,
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
    /// The `#queue` panel (`_queue.html`), pre-rendered — the pending-message FIFO with per-item
    /// remove controls. Empty/hidden unless messages were sent while a job was running (#2).
    pub queue_html: String,
}

/// One row of the pending-message queue panel (`_queue.html`).
pub struct QueuedItem {
    pub id: u64,
    /// A short, single-line preview of the queued message (the full text is sent when it runs).
    pub preview: String,
}

/// Partial: the pending-message queue (`templates/_queue.html`). The `#queue` container always
/// renders (hidden when empty) so out-of-band transcript responses have a target; with `oob = true`
/// the root carries `hx-swap-oob="true"`.
#[derive(Template, WebTemplate)]
#[template(path = "_queue.html")]
pub struct Queue {
    pub slug: String,
    pub items: Vec<QueuedItem>,
    pub oob: bool,
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
    /// The "menu of moves" (docs/06-concepts/skills.md): every visible non-capstone skill.
    pub moves: Vec<MoveChip>,
    /// The swarm angle picker: every candidate attack angle with its default-checked state, so
    /// the owner can aim a swarm instead of always firing the canonical four (#1). The same set
    /// as `moves`; `on` marks `swarm::DEFAULT_ANGLES`.
    pub swarm_angles: Vec<SwarmAngle>,
    /// The most angles one swarm may be aimed at (`swarm::MAX_ANGLES`); the picker disables
    /// further checkboxes at this count so it never offers a selection the route rejects.
    pub max_angles: usize,
    /// How many angles start checked — the caption's count, so it can't drift from the picker.
    pub default_angles: usize,
    /// The ideation spine with each stage's coverage (`concepts::coverage`, ADR-0022).
    pub spine: Vec<SpineStage>,
    /// The suggested next move's skill name ("" when none, or when the swarm is suggested).
    pub next_skill: String,
    /// Why that move — its use-when line.
    pub next_why: String,
    /// The suggestion is "run a swarm" (convergence is the only thing missing before capstone).
    pub next_is_swarm: bool,
    /// Soft wrong-turn warnings; advice only, never a block.
    pub warnings: Vec<String>,
    /// No attack move has run yet — shown as a note by the store button.
    pub untested: bool,
    /// A job is currently running for this idea. Store is a commitment action, so its button
    /// renders `disabled` while busy (a click would only bounce off `try_claim` anyway); the OOB
    /// actions refresh re-enables it once the job finishes or is cancelled.
    pub busy: bool,
    /// The built-in deterministic workflows (D19) — one chip each, next to swarm.
    pub workflows: Vec<WorkflowChip>,
    /// The trailing sentence fragment in the swarm/workflow/extract tooltips ("Runs serially
    /// <this>, so it takes a while.") — precomputed per the active `LlmBackendKind` (like
    /// `availability_hint`'s remedy copy) rather than branching in the template, so the copy is
    /// never a lie about which backend is actually running the call.
    pub backend_note: String,
    /// The Settings audit toggle: the audited build chip warns when it is off.
    pub audit_on: bool,
    /// The audited build chip's worst-case cost line (ADR-0034), empty when `ready-to-build` is
    /// not registered.
    pub build_cost: String,
    /// The Ground workflows (visible chips and the capstone) that will skip their ground stage
    /// because this idea has no sources attached — the caption names them; empty with sources.
    pub ungrounded: Vec<String>,
    /// The lineage head of the idea's build plans, for the "Plan · vN · k open" chip
    /// (docs/adr/0032); `None` until a first plan exists.
    pub plan: Option<PlanChip>,
    pub oob: bool,
}

/// The idea page's way back to its plan: the head version and how many questions it still asks.
pub struct PlanChip {
    pub version: u32,
    pub open: usize,
}

/// One stage of the spine strip.
pub struct SpineStage {
    pub name: &'static str,
    pub done: bool,
}

/// One move chip: the skill name (the route segment) + its tooltip (description + use-when).
pub struct MoveChip {
    pub name: String,
    pub title: String,
}

/// One workflow button: name (the route segment) + its hover title, which carries the description,
/// the worst-case cost line (ADR-0034: the ceiling is shown before running) and, when the idea has
/// no sources, the note that its Ground stage will be skipped.
pub struct WorkflowChip {
    pub name: String,
    pub title: String,
}

/// One checkbox in the swarm angle picker: the angle (a skill name), its tooltip (the same one
/// the move chip carries), and whether it starts checked.
pub struct SwarmAngle {
    pub name: String,
    pub title: String,
    pub on: bool,
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
    /// A build plan a later version revises (docs/adr/0032): listed, but dimmed.
    pub is_superseded: bool,
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
    /// `PROMPT.md` and `plan.md` projections of a build plan, derived at view time.
    pub prompt_md: Option<String>,
    pub attack_plan_md: Option<String>,
    /// The plan workbench (docs/adr/0032), for a build plan only.
    pub plan_work: Option<PlanWorkView>,
    pub recipe: RecipeView,
}

/// An artifact's provenance as R19 shows it (ADR-0040): what made it, whether that skill or
/// workflow changed since, and each lens whose answer was off its output contract.
pub struct RecipeView {
    /// `skill premortem @ 3f2a1c9b8d7e (vault override) · build 0.1.0+abc123`, or
    /// `provenance unknown` for an artifact written before recipes.
    pub line: String,
    /// The template refs, for the line's tooltip.
    pub templates: String,
    /// Why the "recipe changed since" badge shows (the live digest now differs), if it does.
    pub changed: Option<String>,
    pub off_contract: Vec<String>,
}

/// Partial: the plan workbench (`templates/_plan_work.html`, `id="work"`, docs/adr/0032) — the
/// open questions and owner-held tasks of one plan version, each with the field that answers it,
/// plus the re-plan controls. Rendered inside R19's page, and alone as R46's 422 response with
/// the owner's answers and the per-field error kept.
#[derive(Template, WebTemplate)]
#[template(path = "_plan_work.html")]
pub struct PlanWorkView {
    pub slug: String,
    pub stem: String,
    pub version: u32,
    pub revises: Option<String>,
    /// The `Q#`/`T#` ids answered to make this version.
    pub answered: Vec<String>,
    pub superseded_by: Vec<String>,
    pub head: String,
    pub is_head: bool,
    /// Root first, this version last.
    pub lineage: Vec<LineageStep>,
    /// The run header's mode ("quick", "v2 · answers on … · audit not re-run", …).
    pub mode: String,
    pub open: Vec<PlanQuestion>,
    pub blocked: Vec<PlanBlock>,
    /// Whether the idea's state takes answers (D9: only an active discussion); a Stored idea
    /// shows the questions without the forms.
    pub can_answer: bool,
    /// A submission-wide error with no one field to sit beside ("no answer given").
    pub form_error: Option<String>,
}

impl PlanWorkView {
    /// Forms render only on the head of an idea in discussion: answers land on the head alone.
    pub fn is_answerable(&self) -> bool {
        self.is_head && self.can_answer
    }
}

/// One version in the lineage line.
pub struct LineageStep {
    pub stem: String,
    pub version: u32,
}

/// One open question with its answer field.
pub struct PlanQuestion {
    pub id: String,
    pub text: String,
    pub markers: Vec<String>,
    /// The tasks this question holds back.
    pub blocks: Vec<String>,
    /// The owner's words, kept across a rejected submission or carried from a hedged answer.
    pub value: String,
    pub error: Option<String>,
    /// Why the answer in `value` reads as undecided (G2 keeps a hedged answer open).
    pub hedge: Option<&'static str>,
}

/// One task held for the owner: an answer field when an answer can release it, else its reasons.
pub struct PlanBlock {
    pub id: String,
    pub text: String,
    pub blocked_by: Vec<String>,
    pub reasons: Vec<String>,
    pub answerable: bool,
    pub value: String,
    pub error: Option<String>,
    pub hedge: Option<&'static str>,
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
    /// The server's last-known tool list (name + description), from `McpRegistry::known_tools`.
    /// Unlike `text`/`ok`/`errored`, this deliberately survives a page refresh and an unrelated
    /// probe failure — it is a display convenience, not a health claim (see the registry field
    /// doc). Empty means "never successfully probed this process lifetime".
    pub tools: Vec<crate::mcp::ToolSummary>,
}

/// The skill book page shell (`templates/skills.html`); the panel is pre-rendered so a reload
/// can swap just `#skills` — same split as `SourcesPage`/`SourcesList`.
#[derive(Template, WebTemplate)]
#[template(path = "skills.html")]
pub struct SkillsPage {
    pub list_html: String,
}

/// Partial: the swappable skill book panel (`templates/_skills_list.html`) — the owner skills
/// folder + reload, any files that failed to load, and one group per spine stage.
#[derive(Template, WebTemplate)]
#[template(path = "_skills_list.html")]
pub struct SkillsList {
    pub dir: String,
    pub issues: Vec<crate::concepts::skills::SkillIssue>,
    pub groups: Vec<SkillGroup>,
    /// Where owner workflows are read from (`IDEA_VAULT_WORKFLOWS_DIR`, ADR-0035).
    pub workflow_dir: String,
    /// Owner workflow files that failed to load or validate on the last (re)load.
    pub workflow_issues: Vec<crate::concepts::workflows::WorkflowIssue>,
    /// Every registered workflow, in chip order — drawn from the same book snapshot as `groups`.
    pub workflows: Vec<WorkflowCard>,
}

/// One workflow on the skill book and its R49 detail page (ADR-0035), as display strings.
pub struct WorkflowCard {
    pub name: String,
    pub description: String,
    pub use_when: String,
    pub avoid_when: String,
    pub source: &'static str,
    /// The file's digest, as a workflow artifact's recipe records it (ADR-0040).
    pub digest: String,
    pub hidden: bool,
    /// Derived from the definition (it chains a build-plan skill): runs from the capstone row.
    pub capstone: bool,
    /// The worst-case calls and waves line (ADR-0034), the same one the chip's title carries.
    pub cost: String,
    /// It opens with a Ground stage, which is skipped on an idea with no sources.
    pub needs_sources: bool,
    pub stages: Vec<StageLine>,
}

/// One stage of a workflow, as the book and the detail page list it.
pub struct StageLine {
    /// The on-disk `kind:` spelling.
    pub kind: &'static str,
    pub detail: String,
    /// This stage's share of the ceiling.
    pub calls: u32,
    /// A Panel's rubric; empty for every other kind.
    pub rubric: Vec<RubricLine>,
}

/// One Panel criterion with the anchors a scorer reads for 0 and 2.
pub struct RubricLine {
    pub name: String,
    pub weight: u8,
    pub zero: String,
    pub two: String,
}

/// R49 — one workflow in full (`templates/workflow_detail.html`, ADR-0035): its stages with each
/// one's share of the ceiling, the owner-facing explanation, and the file as loaded.
#[derive(Template, WebTemplate)]
#[template(path = "workflow_detail.html")]
pub struct WorkflowDetailPage {
    pub card: WorkflowCard,
    /// The markdown body below the frontmatter, rendered and sanitised.
    pub body_html: String,
    /// The definition file as read.
    pub raw: String,
    pub dir: String,
}

/// One spine stage on the skill book.
pub struct SkillGroup {
    pub stage: &'static str,
    pub blurb: &'static str,
    pub skills: Vec<SkillCard>,
}

/// One skill on the skill book: the frontmatter, as display strings.
pub struct SkillCard {
    pub name: String,
    pub description: String,
    pub use_when: String,
    pub avoid_when: String,
    pub role: &'static str,
    pub contract: &'static str,
    pub source: &'static str,
    /// The file's digest, as an artifact's recipe records it (ADR-0040).
    pub digest: String,
    pub hidden: bool,
}

/// The Sources page shell (`templates/sources.html`); the list is pre-rendered so a mutation can
/// swap just the `#sources` panel — same split as `McpPage`/`McpList`.
#[derive(Template, WebTemplate)]
#[template(path = "sources.html")]
pub struct SourcesPage {
    pub list_html: String,
}

/// Partial: the swappable Sources panel (`templates/_sources_list.html`) — the apply-state banner
/// or quiet note, the registered-source rows, and the add form. Returned by `GET /sources`
/// (embedded) and by every mutating `/sources/*` route (add/update/delete) so the panel reflects
/// the registry without a full reload.
#[derive(Template, WebTemplate)]
#[template(path = "_sources_list.html")]
pub struct SourcesList {
    pub sources: Vec<SourceRow>,
    pub apply: ApplyState,
}

/// One registered source row: the immutable name, the middle-truncated path (full path in the
/// `title` attribute), and the live status pill — probed fresh on every render, never cached
/// (`routes::sources::status_view` maps `SourceStatus` to this text/kind/hint vocabulary).
pub struct SourceRow {
    pub name: String,
    /// `truncate_path_middle`'d for the row; `path_full` carries the whole thing in `title`.
    pub path_display: String,
    pub path_full: String,
    /// The pill text ("mounted (3 entries)", "needs re-up", "readable", …).
    pub status_text: String,
    /// The pill's CSS modifier: `ok` / `stale` (the ADR-0020 ghost-bind warning) / `warn` /
    /// `danger`.
    pub status_kind: &'static str,
    /// The quiet explanation line under a non-ok row; empty (not rendered) when ok.
    pub status_hint: String,
}

/// The saved-vs-applied gap the panel head renders (ADR-0020: the app never runs docker, so the
/// gap between "registered" and "mounted" is the owner's to close — the banner tells them how).
pub struct ApplyState {
    /// How many sources are `NeedsReup` — `> 0` (in container mode) shows the re-up banner.
    pub pending: usize,
    pub total: usize,
    /// The copyable `COMPOSE_FILE=…` `.env` line, derived server-side from the actual vault dir
    /// (`routes::sources::compose_file_line`) so the template never assembles a path.
    pub override_path: String,
    /// No sources mount configured (`IDEA_VAULT_SOURCES_DIR` unset) — statuses read the host
    /// paths directly and there is nothing to re-up.
    pub bare_mode: bool,
    /// Container mode with the override never layered (`IDEA_VAULT_SOURCES_APPLIED` absent) —
    /// show the one-time COMPOSE_FILE setup instruction alongside the re-up command.
    pub show_compose_setup: bool,
}

/// Partial: a single source's view-mode `<li>` (`templates/_source_row.html`). `{% include %}`-d
/// by `_sources_list.html` for every row in the loop — the field name (`source`) matches the
/// loop binding, the same include-scope trick as `McpRow`, which is what lets the edit form's
/// cancel action (`GET /sources/{name}/view`) render exactly one row standalone.
#[derive(Template, WebTemplate)]
#[template(path = "_source_row.html")]
pub struct SourceRowView {
    pub source: SourceRow,
}

/// Partial: one source's edit-mode `<li>` (`templates/_source_edit_row.html`), swapped in by
/// `GET /sources/{name}/edit` over the same `#src-row-<name>` id the view row uses, and posted
/// by `POST /sources/{name}/update`. Host path only — the name is immutable (it is the mount
/// target and the tool routing key, see `SourceRegistry::update_path`).
#[derive(Template, WebTemplate)]
#[template(path = "_source_edit_row.html")]
pub struct SourceEditRow {
    pub name: String,
    pub path: String,
}

/// Partial: the idea's attached-sources row (`templates/_idea_sources.html`) — chips plus the
/// checkbox editor — swapped whole by `POST /idea/{slug}/sources`. The sibling of [`IdeaTags`].
#[derive(Template, WebTemplate)]
#[template(path = "_idea_sources.html")]
pub struct IdeaSources {
    pub slug: String,
    /// One chip per attached name, in frontmatter order (including names no longer in the
    /// registry, flagged — frontmatter is truth).
    pub chips: Vec<SourceChip>,
    /// One checkbox per registered source, plus checked extras for attached-but-unregistered
    /// names. Empty means "registry empty and nothing attached" — the editor shows the
    /// register-one-first line instead of a form.
    pub options: Vec<SourceOption>,
}

/// One attached-source chip. `flag` is the degraded-state suffix ("needs re-up" / "missing");
/// empty means healthy — no suffix span rendered.
pub struct SourceChip {
    pub name: String,
    /// The hover explanation — standard for registered sources, the re-add remedy for names
    /// missing from the registry.
    pub title: String,
    pub flag: String,
    /// CSS tint for the flag: `warn` / `danger` (unused when `flag` is empty).
    pub flag_kind: &'static str,
}

/// One row of the attach editor: a checkbox for a registered source (or a checked extra for an
/// attached-but-unregistered name), with a quiet status hint when the source is degraded.
pub struct SourceOption {
    pub name: String,
    pub attached: bool,
    /// Non-empty when there is something to say ("needs re-up", "not in the registry", …).
    pub hint: String,
}

/// One search hit, pre-rendered for `_search_results.html` from `index::queries::SearchHit` by
/// `routes::ideas::search`: the snippet is escaped-then-highlighted HTML (never the raw
/// sentinel-delimited plain text `SearchHit::snippet` carries — see `routes::ideas::highlight_snippet`
/// for the escape-first XSS-boundary contract), and the provenance kind is resolved to display
/// chip text (empty = no chip) rather than the raw `search_fts.kind` string.
pub struct SearchHitView {
    pub slug: String,
    pub title: String,
    /// Escaped-then-`<mark>`-highlighted HTML — safe to render with `|safe`.
    pub snippet_html: String,
    /// Provenance chip text ("tags" / "memory" / "artifact" / "conversation"); empty for `title`
    /// and `idea_body` hits, which need no chip since the owner already expects a match there.
    pub kind_chip: String,
}

/// Partial: full-text search results (R8, `templates/_search_results.html`).
#[derive(Template, WebTemplate)]
#[template(path = "_search_results.html")]
pub struct SearchResults {
    pub hits: Vec<SearchHitView>,
}
