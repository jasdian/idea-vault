//! The tool catalog (docs/adr/0024) and the synchronous half of tool dispatch.
//!
//! The long-running tools (`chat`, `store_idea`, `run_skill`, `run_swarm`, `build_plan`) are declared [`TaskSupport::Optional`] in [`catalog`]: a `tools/call`
//! with `task:{}` takes the Task lifecycle in `tasks.rs` (`enqueue_task` → `tasks/get` →
//! `tasks/result`), while a plain `tools/call` from a Task-unaware client is routed by
//! `call_sync` to [`super::tasks::TaskRegistry::call_sync_bounded`] — the same claim/spawn and
//! the same terminal cache, plus a short bounded wait (docs/adr/0028).
//!
//! Until ADR-0028 both tools were [`TaskSupport::Required`]: the `rmcp` dispatch layer rejected a
//! plain `tools/call` for either with `-32601 Method not found` before `call_sync` was reached, so
//! `call_sync` only ever handled the synchronous tools. That constraint was relaxed because a
//! client without Tasks support (Claude Code's own MCP client among them) could not call them at
//! all; flipping the two `TaskSupport` values back is the whole revert if the bounded wait ever
//! proves the wrong trade.

use std::sync::Arc;

use rmcp::model::{CallToolResult, Content, JsonObject, TaskSupport, Tool, ToolExecution};
use rmcp::ErrorData as McpError;
use serde_json::{json, Value};

use chrono::Utc;

use crate::concepts::build_plan::workbench::{self, AnswerChannel, AnswerRequest, WorkbenchError};
use crate::index::{self, queries};
use crate::vault::{store, VaultError};
use crate::web::jobs;
use crate::web::routes::ideas::create_idea_core;
use crate::web::routes::memory::{guard_discussion_state, reopen_idea_core};
use crate::web::routes::{reindex_logged, scoped_llm};
use crate::web::state::AppState;

use super::tasks::TaskRegistry;

fn to_schema(v: Value) -> Arc<JsonObject> {
    Arc::new(v.as_object().cloned().unwrap_or_default())
}

fn schema_empty() -> Value {
    json!({ "type": "object", "properties": {}, "additionalProperties": false })
}

/// The optional `idempotency_key` every long-running tool accepts (docs/adr/0033): a retry with
/// the same key and arguments replays the served result instead of starting a second run.
fn idempotency_key_schema() -> Value {
    json!({
        "type": "string",
        "description": "optional: reuse the same key to safely retry — the first run's result is \
                        replayed (for 24 h) instead of running again; a new key forces a fresh run",
    })
}

/// Pull a required, non-blank string argument out of the call's JSON args.
pub(super) fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, McpError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| McpError::invalid_params(format!("missing required argument '{key}'"), None))
}

fn optional_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

/// The full tool catalog (advertised by `tools/list` and consulted by `get_tool` for
/// task-support validation).
pub(super) fn catalog() -> Vec<Tool> {
    vec![
        Tool::new(
            "list_ideas",
            "List every idea in the vault, most recently updated first.",
            to_schema(schema_empty()),
        ),
        Tool::new(
            "get_idea",
            "Read one idea in full: frontmatter, body, the complete conversation transcript, \
             its memory facts (with bodies), the compacted-context summary if any, and its \
             artifact list (read one with get_artifact).",
            to_schema(json!({
                "type": "object",
                "properties": { "slug": { "type": "string", "description": "idea slug" } },
                "required": ["slug"],
                "additionalProperties": false,
            })),
        ),
        Tool::new(
            "search",
            "Full-text search across idea titles, bodies, conversations, and memory.",
            to_schema(json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"],
                "additionalProperties": false,
            })),
        ),
        Tool::new(
            "create_idea",
            "Create a new Draft idea in the vault.",
            to_schema(json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string" },
                    "body": { "type": "string", "description": "optional seed body" },
                },
                "required": ["title"],
                "additionalProperties": false,
            })),
        ),
        Tool::new(
            "reopen_idea",
            "Reopen a Stored idea back into discussion, loading its memory as context.",
            to_schema(json!({
                "type": "object",
                "properties": { "slug": { "type": "string" } },
                "required": ["slug"],
                "additionalProperties": false,
            })),
        ),
        Tool::new(
            "chat",
            "Send one discussion turn to the idea's foil and get its reply. Long-running: prefer \
             invoking it as a task (tools/call with task:{}) and polling tasks/get then \
             tasks/result. A plain call waits a few seconds; if the reply is not ready yet it \
             returns a 'still running' note — call again with the same arguments to collect it.",
            to_schema(json!({
                "type": "object",
                "properties": {
                    "slug": { "type": "string" },
                    "message": { "type": "string" },
                    "idempotency_key": idempotency_key_schema(),
                },
                "required": ["slug", "message"],
                "additionalProperties": false,
            })),
        )
        // Was `TaskSupport::Required` until ADR-0028: a Task-unaware client could not call the
        // tool at all (rmcp rejected the plain call with -32601). `Optional` keeps the task path
        // identical and adds the bounded-wait plain path; revert to `Required` to drop the latter.
        .with_execution(ToolExecution::new().with_task_support(TaskSupport::Optional)),
        Tool::new(
            "store_idea",
            "Consolidate the discussion and extract memory, transitioning the idea to Stored. \
             Long-running: prefer invoking it as a task (tools/call with task:{}) and polling \
             tasks/get then tasks/result. A plain call waits a few seconds; if not finished yet \
             it returns a 'still running' note — call again with the same slug to collect it.",
            to_schema(json!({
                "type": "object",
                "properties": {
                    "slug": { "type": "string" },
                    "idempotency_key": idempotency_key_schema(),
                },
                "required": ["slug"],
                "additionalProperties": false,
            })),
        )
        // Was `TaskSupport::Required` until ADR-0028 — same reasoning as `chat` above.
        .with_execution(ToolExecution::new().with_task_support(TaskSupport::Optional)),
        Tool::new(
            "list_skills",
            "List the skill book: every named ideation move (name, stage, role, description, \
             use-when/avoid-when guidance, source). Pass a name to run_skill, or a non-capstone, \
             non-converge name as a run_swarm angle.",
            to_schema(schema_empty()),
        ),
        Tool::new(
            "run_skill",
            "Apply one named skill (see list_skills) to an in-discussion idea; the foil's move \
             is appended to the conversation and returned. Long-running: prefer invoking it as \
             a task (tools/call with task:{}); a plain call waits a few seconds and otherwise \
             returns a 'still running' note — call again with the same arguments to collect it.",
            to_schema(json!({
                "type": "object",
                "properties": {
                    "slug": { "type": "string" },
                    "name": { "type": "string", "description": "skill name from list_skills" },
                    "idempotency_key": idempotency_key_schema(),
                },
                "required": ["slug", "name"],
                "additionalProperties": false,
            })),
        )
        .with_execution(ToolExecution::new().with_task_support(TaskSupport::Optional)),
        Tool::new(
            "run_swarm",
            "Fan out up to 8 subagents, each attacking the idea from one angle (a skill name), \
             then converge them into one synthesis turn, which is returned. Omit angles for the \
             default set. Long-running (many model calls): prefer invoking it as a task; a plain \
             call waits a few seconds and otherwise returns a 'still running' note — call again \
             with the same arguments to collect it.",
            to_schema(json!({
                "type": "object",
                "properties": {
                    "slug": { "type": "string" },
                    "angles": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "skill names to use as angles; omit for the default set",
                    },
                    "idempotency_key": idempotency_key_schema(),
                },
                "required": ["slug"],
                "additionalProperties": false,
            })),
        )
        .with_execution(ToolExecution::new().with_task_support(TaskSupport::Optional)),
        Tool::new(
            "build_plan",
            "Build (or re-build) the idea's plan: the build-prompt capstone, or with audited:true \
             the ready-to-build workflow (fan-out, audit, then plan). Each run is a new version \
             linked to the current plan, carrying the owner's earlier answers forward. Returns \
             the plan's open questions and owner-blocked tasks — answer them with answer_plan. \
             Long-running: prefer invoking it as a task; a plain call waits a few seconds and \
             otherwise returns a 'still running' note — call again with the same arguments to \
             collect it.",
            to_schema(json!({
                "type": "object",
                "properties": {
                    "slug": { "type": "string" },
                    "audited": {
                        "type": "boolean",
                        "description": "run the audited ready-to-build workflow instead of the \
                                        quick capstone (default false)",
                    },
                    "idempotency_key": idempotency_key_schema(),
                },
                "required": ["slug"],
                "additionalProperties": false,
            })),
        )
        .with_execution(ToolExecution::new().with_task_support(TaskSupport::Optional)),
        Tool::new(
            "get_plan",
            "Read one build-plan version as structured JSON: its lineage, open questions (with \
             the tasks each blocks), owner-blocked tasks (with reasons and whether an answer can \
             release them) and settled items. Defaults to the newest version (the head).",
            to_schema(json!({
                "type": "object",
                "properties": {
                    "slug": { "type": "string", "description": "idea slug" },
                    "plan": {
                        "type": "string",
                        "description": "plan artifact slug; omit for the head",
                    },
                },
                "required": ["slug"],
                "additionalProperties": false,
            })),
        ),
        Tool::new(
            "answer_plan",
            "Answer open questions (Q#) and answerable owner-blocked tasks (T#) on the head plan \
             in the owner's own words. Each answer is saved as the owner's turn and a new plan \
             version is made deterministically (no model call). Relay the owner's words; never \
             compose an answer yourself. Resubmitting identical answers returns the same \
             version.",
            to_schema(json!({
                "type": "object",
                "properties": {
                    "slug": { "type": "string", "description": "idea slug" },
                    "plan": { "type": "string", "description": "the head plan's artifact slug" },
                    "answers": {
                        "type": "object",
                        "description": "id → the owner's words, e.g. {\"Q6\": \"…\", \"T4\": \"…\"}",
                        "additionalProperties": { "type": "string" },
                    },
                },
                "required": ["slug", "plan", "answers"],
                "additionalProperties": false,
            })),
        ),
        Tool::new(
            "get_artifact",
            "Read one markdown artifact of an idea (a swarm/extract finding or synthesis, a \
             build plan version, or quarantined memory facts) by its slug from get_idea's \
             artifact list. For a build plan's structured open questions, use get_plan.",
            to_schema(json!({
                "type": "object",
                "properties": {
                    "slug": { "type": "string", "description": "idea slug" },
                    "artifact": { "type": "string", "description": "artifact slug" },
                },
                "required": ["slug", "artifact"],
                "additionalProperties": false,
            })),
        ),
    ]
}

/// Dispatch a plain (non-task) `tools/call`. The synchronous tools answer inline; the
/// long-running ones take the bounded-wait path on the shared task registry (module doc).
pub(super) async fn call_sync(
    state: &AppState,
    tasks: &TaskRegistry,
    name: &str,
    args: Option<JsonObject>,
) -> Result<CallToolResult, McpError> {
    if matches!(
        name,
        "chat" | "store_idea" | "run_skill" | "run_swarm" | "build_plan"
    ) {
        return tasks.call_sync_bounded(state, name, args).await;
    }
    let args = args.map(Value::Object).unwrap_or(Value::Null);
    match name {
        "list_ideas" => list_ideas(state),
        "get_idea" => get_idea(state, &args),
        "search" => search(state, &args),
        "create_idea" => create_idea(state, &args),
        "reopen_idea" => reopen_idea(state, &args).await,
        "list_skills" => Ok(list_skills(state)),
        "get_artifact" => get_artifact(state, &args),
        "get_plan" => get_plan(state, &args),
        "answer_plan" => answer_plan(state, &args).await,
        _ => Err(McpError::invalid_params(
            format!("unknown tool '{name}'"),
            None,
        )),
    }
}

fn list_ideas(state: &AppState) -> Result<CallToolResult, McpError> {
    let conn = state
        .db
        .lock()
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
    match queries::list_ideas(&conn) {
        Ok(ideas) => {
            let payload: Vec<Value> = ideas
                .into_iter()
                .map(|i| {
                    json!({
                        "slug": i.slug,
                        "title": i.title,
                        "state": i.state,
                        "updated_at": i.updated_at,
                        "tags": i.tags,
                    })
                })
                .collect();
            Ok(CallToolResult::success(vec![Content::text(
                Value::Array(payload).to_string(),
            )]))
        }
        Err(e) => Ok(CallToolResult::error(vec![Content::text(e.to_string())])),
    }
}

fn get_idea(state: &AppState, args: &Value) -> Result<CallToolResult, McpError> {
    let slug = required_str(args, "slug")?;
    match idea_payload(&state.config.vault_dir, slug) {
        Ok(payload) => Ok(CallToolResult::success(vec![Content::text(
            payload.to_string(),
        )])),
        Err(e) => Ok(CallToolResult::error(vec![Content::text(e.to_string())])),
    }
}

/// Everything the idea page shows, as one JSON document — so an MCP client sees the same idea the
/// owner does (fact bodies, not only the MEMORY.md one-liners; artifacts incl. quarantined facts).
fn idea_payload(vault_dir: &std::path::Path, slug: &str) -> Result<Value, VaultError> {
    let idea = store::read_idea(vault_dir, slug)?;
    let conversation = store::read_conversation(vault_dir, slug)?;
    let index = store::read_memory_index(vault_dir, slug)?;
    let facts = store::read_memory_facts(vault_dir, slug)?;
    let compacted = store::read_compacted(vault_dir, slug)?;
    let artifacts = store::read_artifacts(vault_dir, slug)?;
    let html_reports: Vec<String> = store::list_artifact_files(vault_dir, slug)?
        .into_iter()
        .filter(|f| f.ext == store::ArtifactExt::Html)
        .map(|f| f.slug)
        .collect();

    Ok(json!({
        "slug": idea.frontmatter.slug,
        "title": idea.frontmatter.title,
        "state": idea.frontmatter.state.as_str(),
        "tags": idea.frontmatter.tags,
        "sources": idea.frontmatter.sources,
        "created": idea.frontmatter.created.to_rfc3339(),
        "updated": idea.frontmatter.updated.to_rfc3339(),
        "body": idea.body,
        "conversation": conversation,
        "compacted": compacted.map(|c| json!({
            "compacted_through": c.frontmatter.compacted_through,
            "summary": c.summary,
        })),
        "memory": facts.iter().map(|f| json!({
            "slug": f.frontmatter.slug,
            "title": f.frontmatter.title,
            "summary": index
                .entries
                .iter()
                .find(|e| e.slug == f.frontmatter.slug)
                .map(|e| e.summary.as_str()),
            "tags": f.frontmatter.tags,
            "links": f.frontmatter.links,
            "body": f.body,
        })).collect::<Vec<_>>(),
        "artifacts": artifacts.iter().map(|a| json!({
            "slug": a.frontmatter.slug,
            "title": a.frontmatter.title,
            "kind": a.frontmatter.kind.as_str(),
            "lens": a.frontmatter.lens,
            "created": a.frontmatter.created.to_rfc3339(),
        })).collect::<Vec<_>>(),
        // Derived browser-only exports (strict-CSP HTML), listed so the client can point the owner
        // at `/idea/{slug}/artifact/{name}.html`; their markdown twin is the readable truth.
        "html_reports": html_reports,
    }))
}

fn list_skills(state: &AppState) -> CallToolResult {
    let skills = state.skills.snapshot();
    let payload: Vec<Value> = skills
        .visible()
        .map(|s| {
            json!({
                "name": s.name,
                "description": s.description,
                "stage": s.stage.as_str(),
                "role": s.role,
                "use_when": s.use_when,
                "avoid_when": s.avoid_when,
                "source": s.source.as_str(),
            })
        })
        .collect();
    CallToolResult::success(vec![Content::text(Value::Array(payload).to_string())])
}

fn get_artifact(state: &AppState, args: &Value) -> Result<CallToolResult, McpError> {
    let slug = required_str(args, "slug")?;
    let artifact = required_str(args, "artifact")?;
    match store::read_artifact(&state.config.vault_dir, slug, artifact) {
        Ok(a) => Ok(CallToolResult::success(vec![Content::text(
            json!({
                "slug": a.frontmatter.slug,
                "title": a.frontmatter.title,
                "kind": a.frontmatter.kind.as_str(),
                "lens": a.frontmatter.lens,
                "created": a.frontmatter.created.to_rfc3339(),
                "model": a.frontmatter.model,
                "body": a.body,
            })
            .to_string(),
        )])),
        Err(e) => Ok(CallToolResult::error(vec![Content::text(e.to_string())])),
    }
}

/// A plan artifact slug as a client may pass it: with or without the `.md` the web links carry.
fn plan_stem(raw: &str) -> &str {
    raw.strip_suffix(".md").unwrap_or(raw)
}

/// `get_plan` (docs/adr/0033): the workbench's view of one plan version — the same model the
/// artifact page renders, so an MCP client sees exactly what the owner would answer.
fn get_plan(state: &AppState, args: &Value) -> Result<CallToolResult, McpError> {
    let slug = required_str(args, "slug")?;
    let stem = optional_str(args, "plan")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(plan_stem);
    match workbench::plan_view(&state.config.vault_dir, slug, stem) {
        Ok(view) => Ok(CallToolResult::success(vec![Content::text(
            view.to_json().to_string(),
        )])),
        Err(e) => Ok(CallToolResult::error(vec![Content::text(e.to_string())])),
    }
}

/// `answer_plan`'s `answers` object as the workbench takes it: `(id, words)` pairs in id order
/// (Q# before T#, then numerically), so one submission always writes its turns in one order.
fn plan_answers(args: &Value) -> Result<Vec<(String, String)>, McpError> {
    let bad = || {
        McpError::invalid_params(
            "'answers' must be an object of id → the owner's words",
            None,
        )
    };
    let mut answers = args
        .get("answers")
        .and_then(Value::as_object)
        .ok_or_else(bad)?
        .iter()
        .map(|(id, words)| {
            words
                .as_str()
                .map(|w| (id.trim().to_ascii_uppercase(), w.to_string()))
                .ok_or_else(bad)
        })
        .collect::<Result<Vec<_>, _>>()?;
    answers.sort_by_key(|(id, _)| {
        let mut chars = id.chars();
        let letter = chars.next();
        (
            letter,
            chars.as_str().parse::<u32>().unwrap_or(u32::MAX),
            id.clone(),
        )
    });
    Ok(answers)
}

/// `answer_plan` (docs/adr/0032, docs/adr/0033): the plan workbench's deterministic answer path —
/// no model call and no job slot, so it runs inline (on the blocking pool) like the web R46.
/// Refused while a model job runs for the idea, since that job may be about to write the next
/// plan version. Idempotent from the vault itself: an identical resubmission finds the version it
/// made (`reused`), so no replay cache is needed.
async fn answer_plan(state: &AppState, args: &Value) -> Result<CallToolResult, McpError> {
    let slug = required_str(args, "slug")?.to_string();
    let base = plan_stem(required_str(args, "plan")?).to_string();
    let answers = plan_answers(args)?;
    let idea = store::read_idea(&state.config.vault_dir, &slug)
        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
    guard_discussion_state(idea.frontmatter.state)
        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
    if jobs::is_running(&state.jobs, &slug) {
        return Err(McpError::invalid_params(
            format!("idea '{slug}' is busy — the foil is thinking; answer when it finishes"),
            None,
        ));
    }
    let probe = scoped_llm(state, &slug).source_probe();
    let vault_dir = state.config.vault_dir.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        workbench::answer(AnswerRequest {
            vault_dir: &vault_dir,
            idea_slug: &slug,
            base: &base,
            answers: &answers,
            probe: &probe,
            now: Utc::now(),
            via: AnswerChannel::Mcp,
        })
    })
    .await
    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
    match outcome {
        Ok(v) => {
            if !v.reused {
                reindex_logged(state);
            }
            Ok(CallToolResult::success(vec![Content::text(
                json!({
                    "plan": v.stem,
                    "version": v.version,
                    "revises": v.revises,
                    "answered": v.answered,
                    "unblocked": v.unblocked,
                    "still_open": v.still_open,
                    "reused": v.reused,
                })
                .to_string(),
            )]))
        }
        // A vault failure is the server's; everything else is the caller's input, and
        // `Superseded`'s message names the head to answer on instead.
        Err(WorkbenchError::Concept(e)) => Err(McpError::internal_error(e.to_string(), None)),
        Err(e) => Err(McpError::invalid_params(e.to_string(), None)),
    }
}

fn search(state: &AppState, args: &Value) -> Result<CallToolResult, McpError> {
    let query = required_str(args, "query")?;
    let conn = state
        .db
        .lock()
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
    match queries::search(&conn, query) {
        Ok(hits) => {
            let payload: Vec<Value> = hits
                .into_iter()
                .map(|h| {
                    // The snippet carries PUA highlight sentinels meant for HTML `<mark>`
                    // rendering (see `SearchHit::snippet`'s doc) — stripped here for a plain-text
                    // reader, not translated into markup an LLM client has no use for.
                    let snippet = h
                        .snippet
                        .replace([index::SNIPPET_MATCH_OPEN, index::SNIPPET_MATCH_CLOSE], "");
                    json!({
                        "slug": h.slug,
                        "title": h.title,
                        "kind": h.kind,
                        "snippet": snippet,
                    })
                })
                .collect();
            Ok(CallToolResult::success(vec![Content::text(
                Value::Array(payload).to_string(),
            )]))
        }
        Err(e) => Ok(CallToolResult::error(vec![Content::text(e.to_string())])),
    }
}

fn create_idea(state: &AppState, args: &Value) -> Result<CallToolResult, McpError> {
    let title = required_str(args, "title")?;
    let body = optional_str(args, "body").unwrap_or("");
    match create_idea_core(state, title, body) {
        Ok(idea) => Ok(CallToolResult::success(vec![Content::text(
            json!({
                "slug": idea.frontmatter.slug,
                "title": idea.frontmatter.title,
                "state": idea.frontmatter.state.as_str(),
            })
            .to_string(),
        )])),
        Err(e) => Ok(CallToolResult::error(vec![Content::text(e.to_string())])),
    }
}

async fn reopen_idea(state: &AppState, args: &Value) -> Result<CallToolResult, McpError> {
    let slug = required_str(args, "slug")?;
    match reopen_idea_core(state, slug).await {
        Ok(idea) => Ok(CallToolResult::success(vec![Content::text(
            json!({
                "slug": idea.frontmatter.slug,
                "state": idea.frontmatter.state.as_str(),
            })
            .to_string(),
        )])),
        Err(e) => Ok(CallToolResult::error(vec![Content::text(e.to_string())])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_stable_and_marks_long_running_tools_as_task_optional() {
        let tools = catalog();
        assert_eq!(tools.len(), 14);
        let mut seen = std::collections::HashSet::new();
        for t in &tools {
            assert!(
                seen.insert(t.name.clone()),
                "duplicate tool name {}",
                t.name
            );
        }
        // `Required` until ADR-0028 (a Task-unaware client could not call these at all); the
        // Task path is unchanged, the plain path is the bounded wait in `tasks.rs`.
        for name in ["chat", "store_idea", "run_skill", "run_swarm", "build_plan"] {
            let t = tools.iter().find(|t| t.name.as_ref() == name).unwrap();
            assert_eq!(
                t.task_support(),
                TaskSupport::Optional,
                "{name} must be callable both as a task and plainly"
            );
            // `additionalProperties: false` would reject the replay key (docs/adr/0033) otherwise.
            assert!(
                t.input_schema["properties"]
                    .get("idempotency_key")
                    .is_some(),
                "{name} must accept an idempotency_key"
            );
        }
        for name in [
            "list_ideas",
            "get_idea",
            "search",
            "create_idea",
            "reopen_idea",
            "list_skills",
            "get_artifact",
            // Deterministic plan workbench (docs/adr/0032): no model call, so no task.
            "get_plan",
            "answer_plan",
        ] {
            let t = tools.iter().find(|t| t.name.as_ref() == name).unwrap();
            assert_eq!(
                t.task_support(),
                TaskSupport::Forbidden,
                "{name} must stay synchronous"
            );
        }
    }

    #[test]
    fn plan_answers_sorts_by_id_and_rejects_non_strings() {
        let answers =
            plan_answers(&json!({"answers": {"T4": "d", "q10": "c", "Q2": "b"}})).unwrap();
        let ids: Vec<&str> = answers.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["Q2", "Q10", "T4"]);
        assert!(plan_answers(&json!({"answers": {"Q1": 3}})).is_err());
        assert!(plan_answers(&json!({"answers": ["Q1"]})).is_err());
    }

    #[test]
    fn required_str_rejects_missing_and_blank() {
        assert!(required_str(&json!({}), "slug").is_err());
        assert!(required_str(&json!({"slug": "  "}), "slug").is_err());
        assert_eq!(
            required_str(&json!({"slug": "my-idea"}), "slug").unwrap(),
            "my-idea"
        );
    }
}
