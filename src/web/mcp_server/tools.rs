//! The MVP tool catalog (docs/adr/0024) and the synchronous half of tool dispatch.
//!
//! `chat` and `store_idea` are declared [`TaskSupport::Required`] in [`catalog`] — the `rmcp`
//! dispatch layer enforces that requirement before `call_sync` is ever reached for them (a plain
//! `tools/call` for either name gets `-32601 Method not found` from the framework itself), so
//! `call_sync` only ever actually handles the five synchronous tools. The task-mode path for
//! `chat`/`store_idea` lives in `tasks.rs`.

use std::sync::Arc;

use rmcp::model::{CallToolResult, Content, JsonObject, TaskSupport, Tool, ToolExecution};
use rmcp::ErrorData as McpError;
use serde_json::{json, Value};

use crate::app::AppState;
use crate::index::{self, queries};
use crate::vault::store;
use crate::web::routes::ideas::create_idea_core;
use crate::web::routes::memory::reopen_idea_core;

fn to_schema(v: Value) -> Arc<JsonObject> {
    Arc::new(v.as_object().cloned().unwrap_or_default())
}

fn schema_empty() -> Value {
    json!({ "type": "object", "properties": {}, "additionalProperties": false })
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

/// The full MVP tool catalog (advertised by `tools/list` and consulted by `get_tool` for
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
             and its memory index.",
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
            "Send one discussion turn to the idea's foil and get its reply. Long-running: MUST \
             be invoked as a task (tools/call with task:{}); poll tasks/get then tasks/result.",
            to_schema(json!({
                "type": "object",
                "properties": {
                    "slug": { "type": "string" },
                    "message": { "type": "string" },
                },
                "required": ["slug", "message"],
                "additionalProperties": false,
            })),
        )
        .with_execution(ToolExecution::new().with_task_support(TaskSupport::Required)),
        Tool::new(
            "store_idea",
            "Consolidate the discussion and extract memory, transitioning the idea to Stored. \
             Long-running: MUST be invoked as a task (tools/call with task:{}); poll tasks/get \
             then tasks/result.",
            to_schema(json!({
                "type": "object",
                "properties": { "slug": { "type": "string" } },
                "required": ["slug"],
                "additionalProperties": false,
            })),
        )
        .with_execution(ToolExecution::new().with_task_support(TaskSupport::Required)),
    ]
}

/// Dispatch a plain (non-task) `tools/call`. `chat`/`store_idea` never reach here in practice —
/// see the module doc — but are still handled defensively for a client that somehow bypasses the
/// framework's task-support check.
pub(super) async fn call_sync(
    state: &AppState,
    name: &str,
    args: Option<JsonObject>,
) -> Result<CallToolResult, McpError> {
    let args = args.map(Value::Object).unwrap_or(Value::Null);
    match name {
        "list_ideas" => list_ideas(state),
        "get_idea" => get_idea(state, &args),
        "search" => search(state, &args),
        "create_idea" => create_idea(state, &args),
        "reopen_idea" => reopen_idea(state, &args).await,
        "chat" | "store_idea" => Err(McpError::invalid_params(
            format!(
                "'{name}' is long-running — call it with task:{{}} (see tasks/get, tasks/result)"
            ),
            None,
        )),
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
    let vault_dir = &state.config.vault_dir;

    let idea = match store::read_idea(vault_dir, slug) {
        Ok(idea) => idea,
        Err(e) => return Ok(CallToolResult::error(vec![Content::text(e.to_string())])),
    };
    let conversation = match store::read_conversation(vault_dir, slug) {
        Ok(c) => c,
        Err(e) => return Ok(CallToolResult::error(vec![Content::text(e.to_string())])),
    };
    let memory = match store::read_memory_index(vault_dir, slug) {
        Ok(m) => m,
        Err(e) => return Ok(CallToolResult::error(vec![Content::text(e.to_string())])),
    };

    let payload = json!({
        "slug": idea.frontmatter.slug,
        "title": idea.frontmatter.title,
        "state": idea.frontmatter.state.as_str(),
        "tags": idea.frontmatter.tags,
        "sources": idea.frontmatter.sources,
        "created": idea.frontmatter.created.to_rfc3339(),
        "updated": idea.frontmatter.updated.to_rfc3339(),
        "body": idea.body,
        "conversation": conversation,
        "memory": memory.entries.iter().map(|e| json!({
            "slug": e.slug,
            "summary": e.summary,
        })).collect::<Vec<_>>(),
    });
    Ok(CallToolResult::success(vec![Content::text(
        payload.to_string(),
    )]))
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
    fn catalog_is_stable_and_marks_long_running_tools_as_task_required() {
        let tools = catalog();
        assert_eq!(tools.len(), 7);
        let mut seen = std::collections::HashSet::new();
        for t in &tools {
            assert!(
                seen.insert(t.name.clone()),
                "duplicate tool name {}",
                t.name
            );
        }
        for name in ["chat", "store_idea"] {
            let t = tools.iter().find(|t| t.name.as_ref() == name).unwrap();
            assert_eq!(
                t.task_support(),
                TaskSupport::Required,
                "{name} must require task mode"
            );
        }
        for name in [
            "list_ideas",
            "get_idea",
            "search",
            "create_idea",
            "reopen_idea",
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
    fn required_str_rejects_missing_and_blank() {
        assert!(required_str(&json!({}), "slug").is_err());
        assert!(required_str(&json!({"slug": "  "}), "slug").is_err());
        assert_eq!(
            required_str(&json!({"slug": "my-idea"}), "slug").unwrap(),
            "my-idea"
        );
    }
}
