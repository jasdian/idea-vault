//! Ideas route group (docs/09-web-ui.md D17): the idea list (R1), a single idea view (R2),
//! creation (R3), and full-text search (R8).

use axum::extract::{Path, Query, State};
use axum::Form;
use chrono::Utc;
use serde::Deserialize;

use crate::app::AppState;
use crate::domain::{slug as domain_slug, Idea, IdeaFrontmatter, IdeaState, MAX_IDEA_TAGS};
use crate::index::{queries, reindex};
use crate::vault::store;
use crate::web::templates::{IdeaPage, IdeaRow, ListPage, SearchResults};
use crate::web::WebError;

/// Query for R1: an optional `?tag=x` narrows the overview to one tag (the chips link here).
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub tag: Option<String>,
}

/// R1 — `GET /` — the vault overview: every idea, most-recently-updated first; `?tag=x`
/// filters via the tags index (the previously-unrouted `ideas_with_tag` reader).
pub async fn list_page(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> Result<ListPage, WebError> {
    let filter_tag = query
        .tag
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string);
    let ideas = {
        let conn = state
            .db
            .lock()
            .map_err(|e| WebError::Internal(format!("db mutex poisoned: {e}")))?;
        match &filter_tag {
            Some(tag) => queries::ideas_with_tag(&conn, tag)?,
            None => queries::list_ideas(&conn)?,
        }
    };
    Ok(ListPage { ideas, filter_tag })
}

/// Split one transcript turn into (role, markdown content): the first line's `## <role>`
/// heading names the speaker; text without a heading reads as a bare note.
fn turn_role_and_content(turn: &str) -> (String, String) {
    match turn.split_once('\n') {
        Some((first, rest)) if first.starts_with("## ") => (
            first.trim_start_matches("## ").trim().to_string(),
            rest.to_string(),
        ),
        _ => ("note".to_string(), turn.to_string()),
    }
}

/// Turn the stored role heading into a display label + whether it's the owner's turn:
/// `user` → `you`; `assistant` → `foil`; `assistant (skill: premortem)` → `foil · premortem`;
/// `assistant (swarm)` → `foil · swarm`; `assistant (workflow: x)` → `foil · workflow x`.
fn turn_label(role: &str) -> (String, bool) {
    if role == "user" {
        return ("you".to_string(), true);
    }
    if let Some(rest) = role.strip_prefix("assistant") {
        let rest = rest.trim();
        if rest.is_empty() {
            return ("foil".to_string(), false);
        }
        let inner = rest.trim_start_matches('(').trim_end_matches(')');
        let lens = match inner.split_once(':') {
            // Keep the "workflow" kind in the label — a deterministic workflow run and a skill
            // of the same name must stay distinguishable in the transcript.
            Some((kind, v)) if kind.trim() == "workflow" => format!("workflow {}", v.trim()),
            Some((_, v)) => v.trim().to_string(),
            None => inner.to_string(),
        };
        return (format!("foil · {lens}"), false);
    }
    (role.to_string(), false)
}

/// Render each turn of a transcript to HTML. Shared by the discussion pane (which has the text)
/// and [`render_transcript`] (which reads it) so chat/skill/swarm/delete re-render identically.
fn turns_to_html(slug: &str, conversation: &str) -> Result<Vec<String>, WebError> {
    use askama::Template as _;
    store::split_turns(conversation)
        .iter()
        .enumerate()
        .map(|(index, turn)| {
            let (role, content) = turn_role_and_content(turn);
            let (label, is_user) = turn_label(&role);
            crate::web::templates::Turn {
                label,
                is_user,
                content_html: crate::web::templates::render_markdown(&content),
                slug: slug.to_string(),
                index,
            }
            .render()
            .map_err(|e| WebError::Internal(format!("template render: {e}")))
        })
        .collect()
}

/// Minimal HTML-escape for text dropped into server-built markup (error text, model name).
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The quiet usage line: turns, the *effective* context vs budget (summary + verbatim tail when a
/// fold applies, not the raw transcript), and the model in use. Answers "what is being sent and to
/// whom" at a glance — and, crucially, drops after a compaction instead of pinning at full
/// (auto-compact meter honesty, docs/adr/0012).
fn meter_line(
    model: &str,
    turns: usize,
    effective_bytes: usize,
    compacted_through: Option<usize>,
    budget_bytes: usize,
    tools_bytes: usize,
) -> String {
    let kb = effective_bytes.div_ceil(1024);
    let budget_kb = budget_bytes.div_ceil(1024);
    let plural = if turns == 1 { "" } else { "s" };
    // Tool definitions (built-in web tools + enabled MCP servers' schemas, ADR-0017) ride the
    // context too — the meter must not pretend a 20 KB toolbox is free. Shown as its own term
    // so the owner can attribute growth to tools vs transcript.
    let tools = if tools_bytes > 0 {
        format!(" (+{} KB tools)", tools_bytes.div_ceil(1024))
    } else {
        String::new()
    };
    let compacted = match compacted_through {
        Some(k) => format!(" · compacted through turn {k}"),
        None => String::new(),
    };
    format!(
        r#"<div class="meter">{turns} turn{plural} · ~{kb} KB{tools} of ~{budget_kb} KB context · {model}{compacted}</div>"#,
        model = esc(model)
    )
}

/// The server-driven "thinking" indicator. It self-polls `/pending` 1.5s after it lands in the
/// DOM, so it keeps refreshing (and the elapsed count keeps climbing) until the job finishes —
/// and it re-appears on a fresh page load while a job runs, so navigating away never loses it.
///
/// `note` is the orchestrator's live per-step progress ("swarm · attacking 2/4: constraints"); when
/// empty the indicator falls back to the generic "the foil is thinking". A Cancel control posts to
/// `/idea/{slug}/cancel`, aborting the detached task and swapping the (indicator-free) transcript
/// back in.
fn pending_block(slug: &str, secs: u64, note: &str) -> String {
    let status = if note.trim().is_empty() {
        format!("the foil is thinking — {secs}s")
    } else {
        format!("{} — {secs}s", esc(note))
    };
    format!(
        r##"<div class="foil-pending" role="status" aria-live="polite" hx-get="/idea/{slug}/pending" hx-trigger="load delay:1500ms" hx-target="#transcript" hx-swap="innerHTML"><span class="dots" aria-hidden="true"><i></i><i></i><i></i></span><span class="foil-pending__note">{status}</span><form class="foil-cancel" hx-post="/idea/{slug}/cancel" hx-target="#transcript" hx-swap="innerHTML"><button type="submit" class="btn-cancel" title="Stop this run — nothing is saved">cancel</button></form></div>"##
    )
}

fn error_block(message: &str) -> String {
    format!(
        r#"<div class="foil-error" role="alert"><strong>The foil could not respond.</strong> {}</div>"#,
        esc(message)
    )
}

/// A one-shot neutral outcome line (a job that finished fine but changed nothing, e.g. a forced
/// compaction with nothing to fold) — the quiet sibling of [`error_block`]: same transcript slot,
/// no alarm styling.
fn notice_block(message: &str) -> String {
    format!(
        r#"<div class="foil-notice" role="status">{}</div>"#,
        esc(message)
    )
}

/// A bare re-arm poller: keeps the `/pending` poll alive across a beat when the idea is idle (or
/// showing a one-shot error/notice) but the queue still has messages waiting. Same target/swap as
/// the "thinking" indicator, so the next tick reaches `pending`, which drains the next message and
/// re-renders a real indicator. Without this, polling stops the moment a job finishes and a queued
/// message would sit forever.
fn queue_poller(slug: &str) -> String {
    // `r##"…"##` (not `r#"…"#`): the body contains `hx-target="#transcript"`, whose `"#` would
    // otherwise close a single-hash raw string early — same reason `pending_block` uses `r##`.
    format!(
        r##"<div class="queue-poll" aria-hidden="true" hx-get="/idea/{slug}/pending" hx-trigger="load delay:1500ms" hx-target="#transcript" hx-swap="innerHTML"></div>"##
    )
}

/// The complete inner HTML of `#transcript`: the turns, then a job indicator / error block if a
/// job is active, then the usage meter. This is the single renderer every transcript response
/// goes through — the idea page, the poll endpoint, and chat/skill/swarm/delete all emit it, so
/// the view is identical whether freshly loaded or swapped in.
#[allow(clippy::too_many_arguments)]
pub(crate) fn transcript_inner(
    vault_dir: &std::path::Path,
    slug: &str,
    model: &str,
    conversation: &str,
    pending: crate::web::jobs::Pending,
    queued: usize,
    budget_bytes: usize,
    tools_bytes: usize,
) -> Result<String, WebError> {
    use crate::web::jobs::Pending;
    let turns_html = turns_to_html(slug, conversation)?;

    // Effective context (auto-compact, docs/adr/0012): read the sidecar once and resolve the
    // fingerprint. Cheap — one small file read + an O(k) sum, not a full load_context. A corrupt
    // cache reads as absent, so it never breaks the page.
    let all_turns = store::split_turns(conversation);
    let compacted = store::read_compacted(vault_dir, slug).unwrap_or(None);
    let win = crate::memory::compact::effective_window(&all_turns, compacted.as_ref());

    let mut html = String::new();
    if turns_html.is_empty() && matches!(pending, Pending::Idle) {
        html.push_str(
            r#"<p class="empty-thread">No exchange yet. Push the idea below and let the foil break it.</p>"#,
        );
    }
    html.push_str(&turns_html.concat());
    // A Running job carries its own poller (`pending_block`). When the idea is otherwise idle but
    // messages are queued, attach a bare re-arm poller so the poll survives the beat and `pending`
    // can drain the next message. (On a fresh `Idle`+queued render the drainer usually already
    // started a job, so we'd be in the Running arm — this covers the cancel/error transitions.)
    match pending {
        Pending::Running { secs, note } => html.push_str(&pending_block(slug, secs, &note)),
        Pending::Failed(msg) => {
            html.push_str(&error_block(&msg));
            if queued > 0 {
                html.push_str(&queue_poller(slug));
            }
        }
        Pending::Notice(msg) => {
            html.push_str(&notice_block(&msg));
            if queued > 0 {
                html.push_str(&queue_poller(slug));
            }
        }
        Pending::Idle => {
            if queued > 0 {
                html.push_str(&queue_poller(slug));
            }
        }
    }
    // The full transcript is never hidden (every turn is rendered above); this collapsible
    // disclosure just reveals the derived summary the model actually sees for the folded head, so
    // the owner sees exactly what it sees. Only shown when a valid fold applies.
    if win.applied.is_some() {
        if let Some(c) = &compacted {
            html.push_str(&format!(
                r#"<details class="summary-disclosure"><summary>Summary of earlier turns (used for AI context)</summary><div class="summary-disclosure__body">{}</div></details>"#,
                crate::web::templates::render_markdown(&c.summary)
            ));
        }
    }
    html.push_str(&meter_line(
        model,
        all_turns.len(),
        win.effective_bytes,
        win.compacted_through,
        budget_bytes,
        tools_bytes,
    ));
    Ok(html)
}

/// The trailing sentence fragment in the swarm/workflow/extract tooltips, worded for whichever
/// backend is actually going to run the call — see `Actions::backend_note` doc. Split out for the
/// same unit-testability reason `availability_hint` is.
fn backend_note(backend: crate::config::LlmBackendKind) -> String {
    use crate::config::LlmBackendKind;
    let via = match backend {
        LlmBackendKind::Ollama => "on your local Ollama model",
        LlmBackendKind::ClaudeCode => "via claude-code",
    };
    format!("Runs serially {via}, so it takes a while.")
}

/// Render the `#idea-actions` block (`_actions.html`) — the state-dependent moves/swarm/store
/// controls. Shared by the full-page `_discussion.html` render (`oob = false`) and the
/// out-of-band fragment appended to transcript responses (`oob = true`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_actions(
    slug: &str,
    skill_names: Vec<String>,
    can_store: bool,
    busy: bool,
    backend: crate::config::LlmBackendKind,
    oob: bool,
) -> Result<String, WebError> {
    use askama::Template as _;
    // The workflow chips come straight off the static built-in registry — no caller threading.
    let workflows = crate::concepts::workflows::builtin_workflows()
        .iter()
        .map(|w| crate::web::templates::WorkflowChip {
            name: w.name.to_string(),
            description: w.description.to_string(),
        })
        .collect();
    // The swarm angle picker (#1): every move except the `build-prompt` capstone is a candidate
    // attack angle; the canonical `swarm::DEFAULT_ANGLES` start checked. Derived from the moves
    // already threaded in, so no new call-site plumbing — an empty selection falls back to the
    // same defaults server-side (memory::run_swarm), keeping the picker purely additive.
    let swarm_angles = skill_names
        .iter()
        .filter(|n| n.as_str() != "build-prompt")
        .map(|n| crate::web::templates::SwarmAngle {
            name: n.clone(),
            on: crate::concepts::swarm::DEFAULT_ANGLES.contains(&n.as_str()),
        })
        .collect();
    crate::web::templates::Actions {
        slug: slug.to_string(),
        can_store,
        skill_names,
        swarm_angles,
        busy,
        workflows,
        backend_note: backend_note(backend),
        oob,
    }
    .render()
    .map_err(|e| WebError::Internal(format!("template render: {e}")))
}

/// The out-of-band state badge: replaces `#idea-state` in the page subhead so a state flip that
/// happens under a `#transcript`/`#discussion` swap (first chat turn, store, reopen) is visible
/// without a full reload.
pub(crate) fn state_badge_oob(state: IdeaState) -> String {
    let s = state.as_str();
    format!(r#"<span id="idea-state" class="state state--{s}" hx-swap-oob="true">{s}</span>"#)
}

/// Read the current transcript + job state and render it — the response chat/skill/swarm/delete
/// and the poll endpoint return.
///
/// Besides the `#transcript` inner HTML, the response carries two top-level out-of-band fragments
/// re-asserting the current on-disk state: the `#idea-state` badge and the `#idea-actions` block.
/// The first chat turn flips Draft → InDiscussion server-side while the swap only targets
/// `#transcript`; without these the badge and the moves/store controls stay stale until F5.
/// Deliberately OOB (not a wider swap target): the composer sits outside `#transcript` and must
/// survive a poll completing while the owner is typing. Full-page renders go through
/// `transcript_inner` directly and must NOT carry these fragments (duplicate ids).
pub(crate) fn respond_with_transcript(
    state: &AppState,
    slug: &str,
) -> Result<axum::response::Html<String>, WebError> {
    let conversation = store::read_conversation(&state.config.vault_dir, slug)?;
    let pending = crate::web::jobs::peek(&state.jobs, slug);
    let busy = matches!(pending, crate::web::jobs::Pending::Running { .. });
    let queued_items = crate::web::jobs::list_queued(&state.queues, slug);
    let mut html = transcript_inner(
        &state.config.vault_dir,
        slug,
        &state.llm.model(),
        &conversation,
        pending,
        queued_items.len(),
        state.llm.context_budget().max_bytes,
        state.llm.tool_context_bytes(),
    )?;

    let idea = store::read_idea(&state.config.vault_dir, slug)?;
    // The D9 store guard, not the page's `!= Draft` shortcut: delete-turn can reach this on a
    // Stored idea, which must not be offered the discussion controls.
    let can_store = matches!(
        idea.frontmatter.state,
        IdeaState::InDiscussion | IdeaState::Reopened
    );
    let skill_names = state.skills.move_names();
    html.push_str(&state_badge_oob(idea.frontmatter.state));
    html.push_str(&render_actions(
        slug,
        skill_names,
        can_store,
        busy,
        state.llm.settings().backend,
        true,
    )?);
    // Third OOB fragment: the artifacts panel, so a finished extraction (or any transcript
    // refresh) surfaces the new files without a reload — the panel sits outside `#transcript`.
    html.push_str(&crate::web::routes::artifacts::render_artifacts_panel(
        &state.config.vault_dir,
        slug,
        true,
    )?);
    // Fourth OOB fragment: the pending-message queue, so a send/drain/removal reflects live
    // without a reload — the panel sits outside `#transcript`, next to the composer.
    html.push_str(&render_queue_panel(slug, queued_items, true)?);
    Ok(axum::response::Html(html))
}

/// Render the `#queue` panel (`_queue.html`) — the pending-message FIFO with per-item remove.
/// Shared by the transcript OOB refresh (`oob = true`), the full-page discussion render
/// (`oob = false`), and the remove-queued route (`oob = false`, swaps `#queue`).
pub(crate) fn render_queue_panel(
    slug: &str,
    items: Vec<crate::web::jobs::QueuedMessage>,
    oob: bool,
) -> Result<String, WebError> {
    use askama::Template as _;
    let items = items
        .into_iter()
        .map(|m| crate::web::templates::QueuedItem {
            id: m.id,
            preview: preview_line(&m.text),
        })
        .collect();
    crate::web::templates::Queue {
        slug: slug.to_string(),
        items,
        oob,
    }
    .render()
    .map_err(|e| WebError::Internal(format!("template render: {e}")))
}

/// A short, single-line preview of a queued message (the full text is sent when the turn runs).
fn preview_line(text: &str) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = one_line.chars();
    let head: String = chars.by_ref().take(80).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// Render the memory panel (`_memory.html`) — the always-on MEMORY.md index with per-fact delete.
/// Shared by the idea page and the fact-delete route (which swaps `#memory`).
pub(crate) fn render_memory_panel(
    idea_slug: &str,
    entries: Vec<crate::domain::memory::MemoryIndexEntry>,
) -> Result<String, WebError> {
    use askama::Template as _;
    crate::web::templates::MemoryPanel {
        idea_slug: idea_slug.to_string(),
        entries,
    }
    .render()
    .map_err(|e| WebError::Internal(format!("template render: {e}")))
}

/// The poll/cancel responder: the transcript while the idea is in discussion, or — once a store
/// job (R4) has landed truth as `Stored` — the stored view (`_stored.html` + the OOB badge flip).
///
/// The poll indicator targets `#transcript`, but the stored view replaces the whole discussion
/// panel (composer and actions included), so that branch widens the swap with `HX-Retarget`/
/// `HX-Reswap` response headers — exactly the swap the store form performed back when it was a
/// synchronous request. Only the store job can finish on a `Stored` idea (every other job route
/// guards on the discussion states), so the branch fires precisely at store completion.
pub(crate) fn respond_discussion_or_stored(
    state: &AppState,
    slug: &str,
) -> Result<axum::response::Response, WebError> {
    use axum::response::IntoResponse as _;
    let idea = store::read_idea(&state.config.vault_dir, slug)?; // 404 if the idea is gone
    if idea.frontmatter.state == IdeaState::Stored {
        use askama::Template as _;
        let mut html = crate::web::templates::Stored {
            slug: slug.to_string(),
        }
        .render()
        .map_err(|e| WebError::Internal(format!("template render: {e}")))?;
        html.push_str(&state_badge_oob(IdeaState::Stored));
        // The store job rewrote idea.md's body to the consolidated writeup — refresh the page's
        // top .statement out-of-band, since the stored panel deliberately no longer carries it
        // (it would render the same writeup twice on every stored page otherwise).
        html.push_str(&format!(
            r#"<div class="statement" id="idea-statement" hx-swap-oob="true">{}</div>"#,
            crate::web::templates::render_markdown(&idea.body)
        ));
        return Ok((
            [("HX-Retarget", "#discussion"), ("HX-Reswap", "innerHTML")],
            axum::response::Html(html),
        )
            .into_response());
    }
    Ok(respond_with_transcript(state, slug)?.into_response())
}

/// `GET /idea/{slug}/pending` — the poll target: return the current transcript, still carrying the
/// indicator while the job runs, an error once it fails, the finished transcript when done — or
/// the stored view once a store job lands (see [`respond_discussion_or_stored`]).
pub async fn pending(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<axum::response::Response, WebError> {
    // Drain point: if the idea just went idle and a message is queued, start it now so this same
    // poll response carries the fresh "thinking" indicator and the queue advances (no-op when a
    // job is running, an unshown outcome is pending, or nothing is queued).
    crate::web::routes::chat::start_next_queued(&state, &slug);
    respond_discussion_or_stored(&state, &slug)
}

/// `POST /idea/{slug}/cancel` — stop a running job: abort its detached task (dropping the in-flight
/// model future, so nothing partial is persisted) and clear the slot. Returns the transcript with
/// the indicator gone. Idempotent: cancelling when nothing runs just re-renders the current state
/// (including the stored view, if a store job won the race before the cancel arrived).
pub async fn cancel_job(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<axum::response::Response, WebError> {
    store::read_idea(&state.config.vault_dir, &slug)?; // 404 if the idea is gone
    crate::web::jobs::cancel(&state.jobs, &slug);
    respond_discussion_or_stored(&state, &slug)
}

/// `GET /idea/{slug}/history` — the "btw" view: the whole thread on its own page, read-only, with a
/// Fork control. A place to see the full line of thinking and branch off it without derailing it.
pub async fn history_page(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<crate::web::templates::HistoryPage, WebError> {
    let vault_dir = &state.config.vault_dir;
    let idea = store::read_idea(vault_dir, &slug)?; // 404 if missing
    let conversation = store::read_conversation(vault_dir, &slug)?;
    let transcript_html = transcript_inner(
        vault_dir,
        &slug,
        &state.llm.model(),
        &conversation,
        crate::web::jobs::Pending::Idle,
        0, // the read-only history view has no live queue
        state.llm.context_budget().max_bytes,
        state.llm.tool_context_bytes(),
    )?;
    Ok(crate::web::templates::HistoryPage {
        title: idea.frontmatter.title.clone(),
        slug,
        transcript_html,
    })
}

/// `POST /idea/{slug}/fork` — branch this idea into a NEW idea carrying its full context forward
/// (body + conversation + memory), so a tangent can run without disturbing the original. Redirects
/// (HX-Redirect) to the fork.
pub async fn fork_idea(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<axum::response::Response, WebError> {
    use axum::response::IntoResponse;
    let vault_dir = &state.config.vault_dir;
    let src = store::read_idea(vault_dir, &slug)?; // 404 if missing

    // Distinct slug derived from the source, disambiguated against existing idea dirs (D22).
    let base = domain_slug::slugify(&format!("{}-fork", src.frontmatter.title));
    let base = if base.is_empty() {
        format!("{slug}-fork")
    } else {
        base
    };
    let new_slug = domain_slug::disambiguate(&base, |c| vault_dir.join(c).is_dir());

    let now = Utc::now();
    let fork = Idea {
        frontmatter: IdeaFrontmatter {
            title: format!("{} (fork)", src.frontmatter.title),
            slug: new_slug.clone(),
            // It carries a conversation forward, so it opens mid-discussion, not as a blank draft.
            state: IdeaState::InDiscussion,
            tags: src.frontmatter.tags.clone(),
            created: now,
            updated: now,
        },
        body: src.body.clone(),
    };
    store::create_idea(vault_dir, &fork)?;

    // Carry the full context: the whole transcript and every memory fact.
    let conversation = store::read_conversation(vault_dir, &slug)?;
    if !conversation.is_empty() {
        store::append_conversation(vault_dir, &new_slug, &conversation)?;
    }
    for fact in store::read_memory_facts(vault_dir, &slug)? {
        store::write_memory_fact(vault_dir, &new_slug, &fact)?;
    }
    store::rebuild_memory_index(vault_dir, &new_slug)?;
    crate::web::routes::reindex_logged(&state);

    // HTMX full-page navigation to the fork.
    Ok((
        [("HX-Redirect", format!("/idea/{new_slug}"))],
        axum::http::StatusCode::OK,
    )
        .into_response())
}

/// `POST /idea/{slug}/delete` — permanently delete an idea and its whole folder (a deliberate,
/// destructive human action, gated by an explicit confirm in the UI). Redirects home.
pub async fn delete_idea(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<axum::response::Response, WebError> {
    use axum::response::IntoResponse;
    if !store::delete_idea(&state.config.vault_dir, &slug)? {
        return Err(WebError::NotFound(format!("idea: {slug}")));
    }
    crate::web::routes::reindex_logged(&state);
    Ok((
        [("HX-Redirect", "/".to_string())],
        axum::http::StatusCode::OK,
    )
        .into_response())
}

/// Build the discussion pane for any discussion-state idea: rendered transcript turns plus the
/// D20 availability state with its per-state remedy copy. Shared with the reopen route (R5),
/// which returns this partial directly.
/// Resolve the compose-box availability flag + D20 remedy copy for the current backend and health.
/// Split out of [`build_discussion`] so the per-backend wording is unit-testable without a vault.
/// `ModelMissing` is an Ollama-only signal (the claude probe never returns it), so its copy stays
/// Ollama-worded; `Unreachable` is reachable by both backends and so is worded per-backend.
fn availability_hint(
    backend: crate::config::LlmBackendKind,
    health: crate::ai::AiHealth,
    model: &str,
) -> (bool, String) {
    use crate::ai::AiHealth;
    use crate::config::LlmBackendKind;
    match health {
        AiHealth::Available => (true, String::new()),
        AiHealth::ModelMissing => (false, format!("pull a model: `ollama pull {model}`")),
        AiHealth::Unreachable => {
            let hint = match backend {
                LlmBackendKind::Ollama => {
                    "Ollama is not reachable — start it with `ollama serve`".to_string()
                }
                LlmBackendKind::ClaudeCode => "the `claude` CLI isn't runnable — check it's \
                     installed and on the server's PATH, or set IDEA_VAULT_CLAUDE_BIN to its \
                     absolute path"
                    .to_string(),
            };
            (false, hint)
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_discussion(
    vault_dir: &std::path::Path,
    slug: &str,
    conversation: &str,
    health: crate::ai::AiHealth,
    backend: crate::config::LlmBackendKind,
    model: &str,
    can_store: bool,
    skill_names: Vec<String>,
    pending: crate::web::jobs::Pending,
    queued_items: Vec<crate::web::jobs::QueuedMessage>,
    budget_bytes: usize,
    tools_bytes: usize,
) -> Result<crate::web::templates::Discussion, WebError> {
    // D20 per-state remedy copy (docs/05-ai-integration.md).
    let (ai_available, unavailable_hint) = availability_hint(backend, health, model);

    // The #transcript inner is the one shared renderer — so a fresh page load carries the same
    // in-flight indicator (or error) that the poll endpoint would, and mid-job navigation resumes.
    let busy = matches!(pending, crate::web::jobs::Pending::Running { .. });
    let transcript_html = transcript_inner(
        vault_dir,
        slug,
        model,
        conversation,
        pending,
        queued_items.len(),
        budget_bytes,
        tools_bytes,
    )?;
    let actions_html = render_actions(slug, skill_names, can_store, busy, backend, false)?;
    let queue_html = render_queue_panel(slug, queued_items, false)?;

    Ok(crate::web::templates::Discussion {
        slug: slug.to_string(),
        ai_available,
        unavailable_hint,
        transcript_html,
        actions_html,
        queue_html,
    })
}

/// Render the state-dependent lower panel: `_stored.html` for a Stored idea (reopen button),
/// `_discussion.html` (transcript + compose box, disabled when AI is unavailable — D20) for
/// every discussion state. Pre-rendered so the partials stay the single source of truth for
/// both this full page and the HTMX swaps that replace `#discussion` later.
#[allow(clippy::too_many_arguments)]
fn render_panel(
    vault_dir: &std::path::Path,
    idea: &Idea,
    conversation: &str,
    health: crate::ai::AiHealth,
    backend: crate::config::LlmBackendKind,
    model: &str,
    skill_names: Vec<String>,
    pending: crate::web::jobs::Pending,
    queued_items: Vec<crate::web::jobs::QueuedMessage>,
    budget_bytes: usize,
    tools_bytes: usize,
) -> Result<String, WebError> {
    use askama::Template as _;

    if idea.frontmatter.state == IdeaState::Stored {
        return crate::web::templates::Stored {
            slug: idea.frontmatter.slug.clone(),
        }
        .render()
        .map_err(|e| WebError::Internal(format!("template render: {e}")));
    }

    // Store is legal only from InDiscussion/Reopened (D9) — a Draft page must not offer it.
    let can_store = idea.frontmatter.state != IdeaState::Draft;
    build_discussion(
        vault_dir,
        &idea.frontmatter.slug,
        conversation,
        health,
        backend,
        model,
        can_store,
        skill_names,
        pending,
        queued_items,
        budget_bytes,
        tools_bytes,
    )?
    .render()
    .map_err(|e| WebError::Internal(format!("template render: {e}")))
}

/// R2 — `GET /idea/{slug}` — one idea's view: rendered body, memory panel, and the
/// state-dependent discussion/stored panel (docs/09-web-ui.md).
pub async fn idea_page(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<IdeaPage, WebError> {
    let vault_dir = &state.config.vault_dir;
    let idea = store::read_idea(vault_dir, &slug)?; // IdeaNotFound → 404
    let conversation = store::read_conversation(vault_dir, &slug)?;
    let memory_html =
        render_memory_panel(&slug, store::read_memory_index(vault_dir, &slug)?.entries)?;

    // D20: the compose box is disabled (with a per-state remedy banner) unless the model is
    // ready; probing is bounded by the client's 1s hard timeout, so a down Ollama costs at
    // most that per page view.
    let health = state.llm.probe().await;

    let skill_names = state.skills.move_names();
    // If a background job is running for this idea, this resumes its indicator on the fresh page.
    let pending = crate::web::jobs::peek(&state.jobs, &slug);
    // The pending-message queue lives in-process, so it survives navigation and must render on a
    // fresh page load (not only on OOB refreshes).
    let queued_items = crate::web::jobs::list_queued(&state.queues, &slug);
    let panel_html = render_panel(
        vault_dir,
        &idea,
        &conversation,
        health,
        state.llm.settings().backend,
        &state.llm.model(),
        skill_names,
        pending,
        queued_items,
        state.llm.context_budget().max_bytes,
        state.llm.tool_context_bytes(),
    )?;
    let artifacts_html =
        crate::web::routes::artifacts::render_artifacts_panel(vault_dir, &slug, false)?;
    let tags_html = {
        use askama::Template as _;
        render_idea_tags(&idea.frontmatter.slug, &idea.frontmatter.tags)
            .render()
            .map_err(|e| WebError::Internal(format!("template render: {e}")))?
    };
    Ok(IdeaPage {
        title: idea.frontmatter.title.clone(),
        slug: idea.frontmatter.slug.clone(),
        state: idea.frontmatter.state.as_str().to_string(),
        body_html: crate::web::templates::render_markdown(&idea.body),
        memory_html,
        panel_html,
        artifacts_html,
        tags_html,
    })
}

/// Form body for `POST /idea/{slug}/rename` — the new title only. The slug is immutable here (see
/// [`rename_idea`]'s doc); there is no separate field for it.
#[derive(Debug, Deserialize)]
pub struct RenameIdeaForm {
    #[serde(default)]
    pub title: String,
}

/// The rename title cap — generous for a heading, but bounded so a pasted paragraph can't land in
/// `idea.md`'s frontmatter (which the list/search rows and `<title>` all render as one line).
const RENAME_TITLE_MAX_CHARS: usize = 200;

/// `POST /idea/{slug}/rename` — retitle an idea in place. Deliberately **not** a D9 transition:
/// the slug (folder name, `[[slug]]` link target, every `/idea/{slug}` URL) never changes, so
/// backlinks and bookmarks keep working, and the state/body are untouched — this is legal from
/// *every* state, including `Stored` (a finished idea may still want a better title). Truth first
/// (`write_idea` rewrites `idea.md`'s frontmatter `title` + `updated`), then reindex (the list and
/// search rows read titles off the SQLite index, not the file), then re-render just the title
/// block the rename disclosure swaps.
pub async fn rename_idea(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Form(form): Form<RenameIdeaForm>,
) -> Result<crate::web::templates::IdeaTitle, WebError> {
    let title = form.title.trim();
    if title.is_empty() {
        return Err(WebError::BadRequest("title must not be empty".into()));
    }
    if title.chars().count() > RENAME_TITLE_MAX_CHARS {
        return Err(WebError::BadRequest(format!(
            "title is too long ({RENAME_TITLE_MAX_CHARS} characters max)"
        )));
    }
    // Frontmatter titles are single-line by contract: an embedded newline/control char is data-
    // hygiene noise at best and a forged frontmatter line on a future hand edit at worst.
    if title.chars().any(char::is_control) {
        return Err(WebError::BadRequest(
            "title must not contain line breaks or control characters".into(),
        ));
    }

    // Rename is a read-modify-write of the WHOLE idea.md — uncoordinated, it can interleave with
    // a background job's own read…write window (a store job reads idea.md before minutes of model
    // calls and writes the whole struct back after) and silently clobber whichever landed last.
    // Claim the same per-idea slot every job uses: busy ⇒ refuse with a readable message; free ⇒
    // hold it for the few ms the write takes so no job can start mid-rename.
    if !crate::web::jobs::try_claim(&state.jobs, &slug) {
        return Err(WebError::BadRequest(
            "a run is in progress for this idea — wait for it to finish (or cancel it) before renaming"
                .into(),
        ));
    }
    let result = (|| {
        let vault_dir = &state.config.vault_dir;
        let mut idea = store::read_idea(vault_dir, &slug)?; // VaultError::IdeaNotFound -> 404
        idea.frontmatter.title = title.to_string();
        idea.frontmatter.updated = Utc::now();
        store::write_idea(vault_dir, &idea)?;
        Ok::<_, WebError>(idea.frontmatter.title)
    })();
    // Always release the slot — including on the 404/write-error paths — or the idea would read
    // as busy forever.
    crate::web::jobs::mark_done(&state.jobs, &slug);
    let title = result?;

    crate::web::routes::reindex_logged(&state);

    Ok(crate::web::templates::IdeaTitle { title, slug })
}

/// Render the idea's tag row (`_idea_tags.html`) — shared by the full page and the editor swap.
pub(crate) fn render_idea_tags(slug: &str, tags: &[String]) -> crate::web::templates::IdeaTags {
    crate::web::templates::IdeaTags {
        slug: slug.to_string(),
        tags: tags.to_vec(),
        tags_joined: tags.join(", "),
    }
}

/// Form body for `POST /idea/{slug}/tags` — the full comma-separated tag set (replace semantics:
/// the editor shows the current set prefilled, so what the owner saves is what they mean).
#[derive(Debug, Deserialize)]
pub struct TagsForm {
    #[serde(default)]
    pub tags: String,
}

/// `POST /idea/{slug}/tags` — replace the idea's tag set. Tokens are slugified (the tag alphabet
/// is the slug alphabet — that is what reindex, the `kind='tags'` search rows, and the chip URLs
/// all assume); junk tokens drop silently; empty input clears the set. Like rename, this is a
/// whole-file read-modify-write, so it claims the per-idea job slot to never interleave with a
/// running job's own write-back.
pub async fn set_tags(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Form(form): Form<TagsForm>,
) -> Result<crate::web::templates::IdeaTags, WebError> {
    let mut tags: Vec<String> = Vec::new();
    for token in form.tags.split(',') {
        if let Some(tag) = domain_slug::try_slugify(token.trim()) {
            if !tags.iter().any(|t| t == &tag) {
                tags.push(tag);
            }
        }
        if tags.len() == MAX_IDEA_TAGS {
            break;
        }
    }

    if !crate::web::jobs::try_claim(&state.jobs, &slug) {
        return Err(WebError::BadRequest(
            "a run is in progress for this idea — wait for it to finish (or cancel it) before editing tags"
                .into(),
        ));
    }
    let result = (|| {
        let vault_dir = &state.config.vault_dir;
        let mut idea = store::read_idea(vault_dir, &slug)?; // 404 if missing
        idea.frontmatter.tags = tags.clone();
        idea.frontmatter.updated = Utc::now();
        store::write_idea(vault_dir, &idea)?;
        Ok::<_, WebError>(())
    })();
    crate::web::jobs::mark_done(&state.jobs, &slug);
    result?;

    crate::web::routes::reindex_logged(&state);
    Ok(render_idea_tags(&slug, &tags))
}

/// Form body for R3 (the `list.html` new-idea form posts `title`; a seed body is optional).
#[derive(Debug, Deserialize)]
pub struct CreateIdeaForm {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: String,
}

/// R3 — `POST /ideas` — create a new Draft idea (D10) and return its list row partial.
///
/// D10 sequence: validate title non-empty → slugify + collision-check against the vault (D22)
/// → write `idea.md` (state=draft) + empty `conversation.md` (truth first) → index upsert →
/// the `_idea_row.html` partial the list form swaps in.
pub async fn create_idea(
    State(state): State<AppState>,
    Form(form): Form<CreateIdeaForm>,
) -> Result<IdeaRow, WebError> {
    let title = form.title.trim();
    if title.is_empty() {
        return Err(WebError::BadRequest("title must not be empty".into()));
    }

    // D22: slug generated once at creation; the collision check is the atomic directory claim
    // inside `create_idea` — a raced duplicate loses with SlugTaken and we retry with the next
    // candidate, so existing truth can never be silently overwritten.
    let vault_dir = state.config.vault_dir.clone();
    let base = domain_slug::slugify(title);
    let now = Utc::now();
    let mut idea = Idea {
        frontmatter: IdeaFrontmatter {
            title: title.to_string(),
            slug: String::new(),
            state: IdeaState::Draft,
            tags: Vec::new(),
            created: now,
            updated: now,
        },
        body: if form.body.trim().is_empty() {
            String::new()
        } else {
            format!("{}\n", form.body.trim())
        },
    };

    // Truth first: markdown on disk (idea.md + an empty conversation.md per D10), then index.
    let slug = loop {
        let candidate =
            domain_slug::disambiguate(&base, |candidate| vault_dir.join(candidate).is_dir());
        idea.frontmatter.slug = candidate.clone();
        match store::create_idea(&vault_dir, &idea) {
            Ok(()) => break candidate,
            Err(crate::vault::VaultError::SlugTaken(_)) => continue,
            Err(e) => return Err(e.into()),
        }
    };

    // Index upsert. Full transactional rebuild is the canonical correct path (ADR-0002); a
    // per-idea incremental upsert is a future optimization once vault sizes warrant it. An
    // index failure is logged, not surfaced: the markdown truth already landed and the next
    // reindex reconciles (docs/03 "Consistency & failure model").
    {
        let mut conn = state
            .db
            .lock()
            .map_err(|e| WebError::Internal(format!("db mutex poisoned: {e}")))?;
        if let Err(e) = reindex::reindex(&mut conn, &vault_dir) {
            tracing::warn!(error = %e, slug, "index upsert after create failed; truth intact");
        }
    }

    Ok(IdeaRow {
        idea: queries::IdeaSummary {
            slug,
            title: idea.frontmatter.title.clone(),
            state: idea.frontmatter.state.as_str().to_string(),
            tags: idea.frontmatter.tags.clone(),
            updated_at: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        },
    })
}

/// Query string for R8 search.
#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    #[serde(default)]
    pub q: String,
}

/// Turn a search hit's sentinel-delimited plain-text snippet (`index::SNIPPET_MATCH_OPEN`/
/// `SNIPPET_MATCH_CLOSE`, see the contract doc on those constants and on
/// [`queries::SearchHit::snippet`]) into safe, highlighted HTML.
///
/// **This ordering is the XSS boundary: HTML-escape the *entire* snippet FIRST, then translate the
/// sentinel pair into `<mark>`/`</mark>`.** Escaping first neutralizes every byte that originated in
/// owner/AI-authored vault content — none of it can become live markup — and only *after* that do
/// we splice in our own hardcoded, trusted `<mark>` tags around the matched span. Doing it in the
/// other order (mark first, escape second) would let the escape pass mangle our own tags right back
/// into inert text (`<mark>` → `&lt;mark&gt;`), silently defeating the highlight — or worse, if
/// escaping were skipped after marking, would let matched *content* itself carry live HTML.
///
/// A stray/unmatched sentinel is stripped, never rendered: an open with no following close is
/// emitted as plain (already-escaped) text, not left as a dangling `<mark>`; a close with no
/// preceding open is dropped. `reindex::sanitized` already strips literal sentinel codepoints from
/// every string that reaches `search_fts`, so this is belt-and-suspenders against a future
/// searchable surface skipping that step, not the primary defense.
pub(crate) fn highlight_snippet(snippet: &str) -> String {
    use crate::index::{SNIPPET_MATCH_CLOSE, SNIPPET_MATCH_OPEN};
    let escaped = esc(snippet);
    let mut out = String::with_capacity(escaped.len());
    let mut in_mark = false;
    for ch in escaped.chars() {
        match ch {
            SNIPPET_MATCH_OPEN if !in_mark => {
                out.push_str("<mark>");
                in_mark = true;
            }
            SNIPPET_MATCH_CLOSE if in_mark => {
                out.push_str("</mark>");
                in_mark = false;
            }
            // A stray sentinel — a close with no open, or a nested/duplicate open while already
            // marking — is dropped rather than emitted or allowed to reopen a second span.
            SNIPPET_MATCH_OPEN | SNIPPET_MATCH_CLOSE => {}
            _ => out.push(ch),
        }
    }
    if in_mark {
        // An unterminated open (should not happen — `snippet()` always emits balanced pairs):
        // close it out rather than leave a dangling unclosed tag in the response.
        out.push_str("</mark>");
    }
    out
}

/// The provenance chip text for a search hit's best-matching `kind` — empty (no chip) for `title`
/// and `idea_body`, the two surfaces the owner already expects a match to live in; a chip only for
/// the surfaces where "why did this idea match?" would otherwise be invisible (a tag, a distilled
/// memory fact, an AI-generated artifact, or a conversation turn rather than the idea statement).
fn kind_chip(kind: &str) -> String {
    match kind {
        "tags" | "memory" | "artifact" | "conversation" => kind.to_string(),
        _ => String::new(), // "title", "idea_body", and any future/unknown kind: no chip.
    }
}

/// R8 — `GET /search?q=` — full-text search results fragment.
pub async fn search(
    State(state): State<AppState>,
    Query(query): Query<SearchQuery>,
) -> Result<SearchResults, WebError> {
    let hits = {
        let conn = state
            .db
            .lock()
            .map_err(|e| WebError::Internal(format!("db mutex poisoned: {e}")))?;
        // queries::search compiles any input to an injection-proof FTS MATCH expression;
        // empty/whitespace input yields no hits (the fragment renders its empty state).
        queries::search(&conn, &query.q)?
    };
    let hits = hits
        .into_iter()
        .map(|h| crate::web::templates::SearchHitView {
            slug: h.slug,
            title: h.title,
            snippet_html: highlight_snippet(&h.snippet),
            kind_chip: kind_chip(&h.kind),
        })
        .collect();
    Ok(SearchResults { hits })
}

#[cfg(test)]
mod tests {
    use super::{availability_hint, highlight_snippet, kind_chip};
    use crate::ai::AiHealth;
    use crate::config::LlmBackendKind;
    use crate::index::{SNIPPET_MATCH_CLOSE, SNIPPET_MATCH_OPEN};

    #[test]
    fn highlight_snippet_wraps_the_matched_span_in_mark() {
        let raw = format!("before {SNIPPET_MATCH_OPEN}zebra{SNIPPET_MATCH_CLOSE} after");
        assert_eq!(highlight_snippet(&raw), "before <mark>zebra</mark> after");
    }

    #[test]
    fn highlight_snippet_escapes_html_before_marking_never_after() {
        // The XSS boundary: hostile content escaped, but our own <mark> tags survive intact —
        // proof the escape pass ran before the sentinel-to-markup translation, not after.
        let raw = format!(
            "<script>alert(1)</script> {SNIPPET_MATCH_OPEN}<b>zebra</b>{SNIPPET_MATCH_CLOSE}"
        );
        let out = highlight_snippet(&raw);
        assert!(!out.contains("<script>"), "{out}");
        assert!(out.contains("&lt;script&gt;"), "{out}");
        assert!(
            out.contains("<mark>&lt;b&gt;zebra&lt;/b&gt;</mark>"),
            "matched span content must be escaped too, just wrapped in a real <mark>: {out}"
        );
    }

    #[test]
    fn highlight_snippet_drops_stray_sentinels_without_faking_a_mark() {
        // A close with no preceding open: dropped, not rendered as a bogus </mark>.
        let stray_close = format!("plain {SNIPPET_MATCH_CLOSE}text");
        assert_eq!(highlight_snippet(&stray_close), "plain text");

        // A lone open with no close: emitted as plain text (not left as a dangling raw sentinel
        // or a fake opening tag with no visible highlight).
        let stray_open = format!("plain {SNIPPET_MATCH_OPEN}text");
        let out = highlight_snippet(&stray_open);
        assert!(!out.contains(SNIPPET_MATCH_OPEN), "{out}");
        assert!(out.contains("text"), "{out}");
    }

    #[test]
    fn highlight_snippet_never_emits_raw_sentinel_codepoints() {
        let raw = format!("{SNIPPET_MATCH_OPEN}hit{SNIPPET_MATCH_CLOSE}");
        let out = highlight_snippet(&raw);
        assert!(!out.contains(SNIPPET_MATCH_OPEN));
        assert!(!out.contains(SNIPPET_MATCH_CLOSE));
    }

    #[test]
    fn kind_chip_is_empty_for_title_and_body_shown_for_the_rest() {
        assert_eq!(kind_chip("title"), "");
        assert_eq!(kind_chip("idea_body"), "");
        assert_eq!(kind_chip("tags"), "tags");
        assert_eq!(kind_chip("memory"), "memory");
        assert_eq!(kind_chip("artifact"), "artifact");
        assert_eq!(kind_chip("conversation"), "conversation");
        // An unrecognized future kind degrades to no chip rather than a raw internal label.
        assert_eq!(kind_chip("something-new"), "");
    }

    #[test]
    fn meter_line_shows_the_tools_term_only_when_tools_ride_the_context() {
        // Tool definitions consume the window too (ADR-0017): a 13-tool MCP server is ~10 KB of
        // schemas per turn, and the meter must attribute that instead of reporting "~1 KB".
        let with = super::meter_line("m", 2, 1024, None, 64 * 1024, 10 * 1024);
        assert!(
            with.contains("~1 KB (+10 KB tools) of ~64 KB context"),
            "{with}"
        );
        let without = super::meter_line("m", 2, 1024, None, 64 * 1024, 0);
        assert!(without.contains("~1 KB of ~64 KB context"), "{without}");
        assert!(!without.contains("tools"));
    }

    #[test]
    fn available_enables_compose_with_no_hint() {
        let (ok, hint) = availability_hint(LlmBackendKind::Ollama, AiHealth::Available, "qwen");
        assert!(ok);
        assert!(hint.is_empty());
    }

    #[test]
    fn ollama_unreachable_points_at_ollama() {
        let (ok, hint) = availability_hint(LlmBackendKind::Ollama, AiHealth::Unreachable, "qwen");
        assert!(!ok);
        assert!(hint.contains("ollama serve"));
    }

    #[test]
    fn claude_unreachable_names_the_cli_not_ollama() {
        // The bug this guards: a claude-code run showing "start it with `ollama serve`".
        let (ok, hint) =
            availability_hint(LlmBackendKind::ClaudeCode, AiHealth::Unreachable, "opus");
        assert!(!ok);
        assert!(
            hint.contains("claude"),
            "hint should name the claude CLI: {hint}"
        );
        assert!(hint.contains("IDEA_VAULT_CLAUDE_BIN"));
        assert!(!hint.contains("ollama"), "must not blame Ollama: {hint}");
    }

    #[test]
    fn model_missing_is_ollama_pull_copy() {
        let (ok, hint) = availability_hint(LlmBackendKind::Ollama, AiHealth::ModelMissing, "qwen3");
        assert!(!ok);
        assert!(hint.contains("ollama pull qwen3"));
    }
}
