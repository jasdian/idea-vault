//! Inbound MCP server (docs/adr/0024): end-to-end coverage of `/api/mcp` — auth, the MVP tool
//! catalog, the `chat`/`store_idea` Task↔Job bridge, and the markdown-is-truth / reindex
//! invariants a mutating tool must uphold exactly like the HTTP routes do.
//!
//! Drives the real `axum::Router` in-process via `tower::ServiceExt::oneshot`, the same pattern
//! the sibling `mcp-server` repo's own MCP protocol tests use. The router (and the
//! `LocalSessionManager`/`TaskRegistry` state living inside its mounted `StreamableHttpService`)
//! is built ONCE per test and reused via `Router::clone()` — a fresh `build_router` call would
//! mint a fresh, empty session/task registry and break the handshake.

mod support;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use idea_vault::app::build_router;
use idea_vault::index::queries;
use serde_json::{json, Value};
use support::web::{test_state, test_state_with_ollama, with_mcp_token};
use support::{spawn, ChatScript};
use tower::ServiceExt;

const TOKEN: &str = "test-mcp-token";

fn mcp_request(body: Value, session: Option<&str>, auth: Option<&str>) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri("/api/mcp")
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    if let Some(token) = auth {
        b = b.header("authorization", format!("Bearer {token}"));
    }
    if let Some(sid) = session {
        b = b.header("mcp-session-id", sid);
    }
    b.body(Body::from(body.to_string())).unwrap()
}

/// Pull the JSON-RPC payload out of either a raw JSON body or an SSE `data:` line (the
/// streamable-http transport's default response shape).
fn extract_json(raw: &str) -> Value {
    for line in raw.lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                return v;
            }
        }
    }
    serde_json::from_str(raw).unwrap_or_else(|_| panic!("not JSON or SSE data:\n{raw}"))
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Option<String>, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let session = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let bytes = resp.into_body().collect().await.expect("body").to_bytes();
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    let json = if raw.trim().is_empty() {
        Value::Null
    } else {
        extract_json(&raw)
    };
    (status, session, json)
}

/// `initialize` → capture the session id → `notifications/initialized`. Every subsequent call in
/// a test must carry the returned session id.
async fn handshake(app: &Router) -> String {
    let init = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "idea-vault-test", "version": "0" }
        }
    });
    let (status, session, body) = send(app, mcp_request(init, None, Some(TOKEN))).await;
    assert_eq!(status, StatusCode::OK, "initialize failed: {body}");
    let session = session.expect("initialize must return an mcp-session-id header");

    let notif = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    let (status, _, _) = send(app, mcp_request(notif, Some(&session), Some(TOKEN))).await;
    assert!(status.is_success(), "notifications/initialized rejected");
    session
}

async fn call_tool(app: &Router, session: &str, name: &str, args: Value) -> Value {
    let req = json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": { "name": name, "arguments": args }
    });
    let (status, _, body) = send(app, mcp_request(req, Some(session), Some(TOKEN))).await;
    assert_eq!(status, StatusCode::OK, "tools/call {name} failed: {body}");
    body["result"].clone()
}

/// Tool content is a `CallToolResult { content: [{type:"text", text: "<json>"}], ... }` — every
/// MVP tool's success payload is JSON text; this pulls it back out as a `Value`.
fn tool_json(result: &Value) -> Value {
    let text = result["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("tool result had no text content: {result}"));
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()))
}

#[tokio::test]
async fn unauthenticated_request_is_401() {
    let (state, _vault) = test_state();
    let state = with_mcp_token(state, TOKEN);
    let app = build_router(state);

    let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} });
    let (status, _, body) = send(&app, mcp_request(init.clone(), None, None)).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "missing token must 401: {body}"
    );

    let (status, _, body) = send(&app, mcp_request(init, None, Some("wrong-token"))).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "wrong token must 401: {body}"
    );
}

#[tokio::test]
async fn tools_list_includes_the_mvp_catalog() {
    let (state, _vault) = test_state();
    let state = with_mcp_token(state, TOKEN);
    let app = build_router(state);
    let session = handshake(&app).await;

    let req = json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": {} });
    let (status, _, body) = send(&app, mcp_request(req, Some(&session), Some(TOKEN))).await;
    assert_eq!(status, StatusCode::OK);

    let names: Vec<&str> = body["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for expected in [
        "list_ideas",
        "get_idea",
        "search",
        "create_idea",
        "reopen_idea",
        "chat",
        "store_idea",
    ] {
        assert!(
            names.contains(&expected),
            "missing tool '{expected}': {names:?}"
        );
    }
}

#[tokio::test]
async fn create_idea_then_get_idea_and_list_ideas_round_trip_and_the_index_reflects_it() {
    let (state, vault_dir) = test_state();
    let state = with_mcp_token(state, TOKEN);
    let db = state.db.clone();
    let app = build_router(state);
    let session = handshake(&app).await;

    let created = call_tool(
        &app,
        &session,
        "create_idea",
        json!({ "title": "My MCP Idea", "body": "seed body" }),
    )
    .await;
    let created = tool_json(&created);
    let slug = created["slug"].as_str().unwrap().to_string();
    assert_eq!(created["state"], "draft");

    // Markdown is truth: the folder is really on disk.
    assert!(vault_dir.join(&slug).join("idea.md").is_file());

    // The reindex invariant (CLAUDE.md): the SQLite index reflects the new idea immediately,
    // not just on the next manual reindex.
    let indexed = {
        let conn = db.lock().unwrap();
        queries::list_ideas(&conn).unwrap()
    };
    assert!(
        indexed.iter().any(|i| i.slug == slug),
        "new idea not in the index: {indexed:?}"
    );

    let fetched = call_tool(&app, &session, "get_idea", json!({ "slug": slug })).await;
    let fetched = tool_json(&fetched);
    assert_eq!(fetched["title"], "My MCP Idea");
    assert_eq!(fetched["body"], "seed body\n");
    assert_eq!(fetched["state"], "draft");

    let listed = call_tool(&app, &session, "list_ideas", json!({})).await;
    let listed = tool_json(&listed);
    assert!(listed.as_array().unwrap().iter().any(|i| i["slug"] == slug));
}

#[tokio::test]
async fn get_idea_on_a_missing_slug_is_a_tool_error_not_a_protocol_error() {
    let (state, _vault) = test_state();
    let state = with_mcp_token(state, TOKEN);
    let app = build_router(state);
    let session = handshake(&app).await;

    let result = call_tool(&app, &session, "get_idea", json!({ "slug": "nope" })).await;
    assert_eq!(
        result["isError"], true,
        "unknown slug must be a tool-result error: {result}"
    );
}

#[tokio::test]
async fn chat_must_be_invoked_as_a_task() {
    let (state, _vault) = test_state();
    let state = with_mcp_token(state, TOKEN);
    let app = build_router(state);
    let session = handshake(&app).await;

    call_tool(&app, &session, "create_idea", json!({ "title": "Chatty" })).await;

    // A plain (non-task) tools/call for a Required-task tool never reaches our handler at all —
    // the rmcp dispatch layer itself rejects it with -32601 before `call_tool` runs.
    let req = json!({
        "jsonrpc": "2.0", "id": 9, "method": "tools/call",
        "params": { "name": "chat", "arguments": { "slug": "chatty", "message": "hi" } }
    });
    let (_, _, body) = send(&app, mcp_request(req, Some(&session), Some(TOKEN))).await;
    assert_eq!(
        body["error"]["code"], -32601,
        "expected method-not-found: {body}"
    );
}

#[tokio::test]
async fn chat_as_a_task_round_trips_through_tasks_get_and_tasks_result() {
    let mock = spawn(
        &["llama3.2"],
        ChatScript::Tokens(vec!["a foil reply".into()]),
    )
    .await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    let state = with_mcp_token(state, TOKEN);
    let app = build_router(state);
    let session = handshake(&app).await;

    let created = tool_json(
        &call_tool(
            &app,
            &session,
            "create_idea",
            json!({ "title": "Task Chat" }),
        )
        .await,
    );
    let slug = created["slug"].as_str().unwrap().to_string();

    let enqueue = json!({
        "jsonrpc": "2.0", "id": 10, "method": "tools/call",
        "params": {
            "name": "chat",
            "arguments": { "slug": slug, "message": "steelman this" },
            "task": {},
        }
    });
    let (status, _, body) = send(&app, mcp_request(enqueue, Some(&session), Some(TOKEN))).await;
    assert_eq!(status, StatusCode::OK, "enqueue_task failed: {body}");
    let task_id = body["result"]["task"]["taskId"]
        .as_str()
        .unwrap_or_else(|| panic!("no taskId in CreateTaskResult: {body}"))
        .to_string();
    assert_eq!(body["result"]["task"]["status"], "working");

    // Poll tasks/get until the job leaves "working".
    let mut status_str = "working".to_string();
    for _ in 0..300 {
        let get = json!({
            "jsonrpc": "2.0", "id": 11, "method": "tasks/get",
            "params": { "taskId": task_id }
        });
        let (_, _, body) = send(&app, mcp_request(get, Some(&session), Some(TOKEN))).await;
        status_str = body["result"]["status"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if status_str != "working" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        status_str, "completed",
        "chat task did not complete in time"
    );

    let result_req = json!({
        "jsonrpc": "2.0", "id": 12, "method": "tasks/result",
        "params": { "taskId": task_id }
    });
    let (_, _, body) = send(&app, mcp_request(result_req, Some(&session), Some(TOKEN))).await;
    let reply_text = body["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        reply_text.contains("a foil reply"),
        "unexpected reply payload: {body}"
    );

    // Markdown is truth: the reply really landed in conversation.md.
    let conversation =
        std::fs::read_to_string(vault_dir.join(&slug).join("conversation.md")).unwrap();
    assert!(conversation.contains("a foil reply"));
}

#[tokio::test]
async fn store_idea_as_a_task_flips_state_to_stored() {
    let mock = spawn_store_ready_mock().await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    let state = with_mcp_token(state, TOKEN);
    let app = build_router(state);
    let session = handshake(&app).await;

    let created = tool_json(
        &call_tool(
            &app,
            &session,
            "create_idea",
            json!({ "title": "Store Me" }),
        )
        .await,
    );
    let slug = created["slug"].as_str().unwrap().to_string();

    // store_idea needs an InDiscussion/Reopened idea with at least one turn (D9) — write both
    // directly rather than spending a chat round-trip on it.
    idea_vault::vault::store::append_turn(&vault_dir, &slug, "user", "the idea in full").unwrap();
    let mut idea = idea_vault::vault::store::read_idea(&vault_dir, &slug).unwrap();
    idea.frontmatter.state = idea_vault::domain::IdeaState::InDiscussion;
    idea_vault::vault::store::write_idea(&vault_dir, &idea).unwrap();

    let enqueue = json!({
        "jsonrpc": "2.0", "id": 20, "method": "tools/call",
        "params": { "name": "store_idea", "arguments": { "slug": slug }, "task": {} }
    });
    let (status, _, body) = send(&app, mcp_request(enqueue, Some(&session), Some(TOKEN))).await;
    assert_eq!(status, StatusCode::OK, "enqueue_task failed: {body}");
    let task_id = body["result"]["task"]["taskId"]
        .as_str()
        .unwrap()
        .to_string();

    let mut status_str = "working".to_string();
    for _ in 0..500 {
        let get = json!({
            "jsonrpc": "2.0", "id": 21, "method": "tasks/get",
            "params": { "taskId": task_id }
        });
        let (_, _, body) = send(&app, mcp_request(get, Some(&session), Some(TOKEN))).await;
        status_str = body["result"]["status"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if status_str != "working" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        status_str, "completed",
        "store task did not complete in time"
    );

    let idea = idea_vault::vault::store::read_idea(&vault_dir, &slug).unwrap();
    assert_eq!(
        idea.frontmatter.state,
        idea_vault::domain::IdeaState::Stored
    );
}

/// The store pipeline makes two model calls (consolidate, then extract memory) — a sequence mock
/// answering both with a minimal-but-valid completion for each phase.
async fn spawn_store_ready_mock() -> support::MockOllama {
    support::spawn_sequence(
        &["llama3.2"],
        vec![
            ChatScript::Tokens(vec!["## Consolidated\n\nthe idea in full".into()]),
            ChatScript::Tokens(vec!["no new facts".into()]),
        ],
    )
    .await
}

/// Regression for the Task↔Job bridge's terminal-state cache: `web::jobs::peek` is a one-shot
/// consuming read, so without caching, the first `tasks/get` after a failure would consume the
/// `Failed` slot and every later call (a second `tasks/get`, or `tasks/result`) would see `Idle`
/// and fabricate a false success — for `chat` specifically, the user's own last message read back
/// as if it were the assistant's reply.
#[tokio::test]
async fn chat_task_failure_is_reported_as_failed_not_a_false_success() {
    // test_state()'s default Ollama URL is a refused port — the chat call fails fast and
    // deterministically, with no mock server to script.
    let (state, vault_dir) = test_state();
    let state = with_mcp_token(state, TOKEN);
    let app = build_router(state);
    let session = handshake(&app).await;

    let created = tool_json(
        &call_tool(
            &app,
            &session,
            "create_idea",
            json!({ "title": "Doomed Chat" }),
        )
        .await,
    );
    let slug = created["slug"].as_str().unwrap().to_string();

    let enqueue = json!({
        "jsonrpc": "2.0", "id": 50, "method": "tools/call",
        "params": { "name": "chat", "arguments": { "slug": slug, "message": "hi" }, "task": {} }
    });
    let (_, _, body) = send(&app, mcp_request(enqueue, Some(&session), Some(TOKEN))).await;
    let task_id = body["result"]["task"]["taskId"]
        .as_str()
        .unwrap()
        .to_string();

    let mut status_str = "working".to_string();
    for _ in 0..300 {
        let get = json!({
            "jsonrpc": "2.0", "id": 51, "method": "tasks/get",
            "params": { "taskId": task_id }
        });
        let (_, _, body) = send(&app, mcp_request(get, Some(&session), Some(TOKEN))).await;
        status_str = body["result"]["status"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if status_str != "working" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        status_str, "failed",
        "expected the chat job to fail against a refused Ollama port"
    );

    // A SECOND tasks/get on the same task id must report the SAME terminal status — this is the
    // exact call that used to roll a consumed Failed slot over to a false "completed".
    let get_again = json!({
        "jsonrpc": "2.0", "id": 52, "method": "tasks/get",
        "params": { "taskId": task_id }
    });
    let (_, _, body) = send(&app, mcp_request(get_again, Some(&session), Some(TOKEN))).await;
    assert_eq!(
        body["result"]["status"], "failed",
        "second tasks/get must not roll a Failed job over to completed: {body}"
    );

    let result_req = json!({
        "jsonrpc": "2.0", "id": 53, "method": "tasks/result",
        "params": { "taskId": task_id }
    });
    let (_, _, body) = send(&app, mcp_request(result_req, Some(&session), Some(TOKEN))).await;
    assert_eq!(
        body["result"]["isError"], true,
        "a failed chat task must surface as a tool-result error, not a false success: {body}"
    );

    // No fabricated assistant turn ever landed in conversation.md.
    let conversation =
        std::fs::read_to_string(vault_dir.join(&slug).join("conversation.md")).unwrap();
    assert!(
        !conversation.contains("## assistant"),
        "a failed turn must not persist an assistant reply: {conversation}"
    );
}

/// Regression: `tasks/cancel` must record a terminal outcome of its own, so a later `tasks/get`
/// or `tasks/result` on the same task id reports `cancelled` forever after — not `completed` with
/// a fabricated success payload re-derived from whatever the vault happens to look like once the
/// aborted job's slot is gone.
#[tokio::test]
async fn cancelled_task_is_reported_as_cancelled_not_a_false_success() {
    let mock = spawn(
        &["llama3.2"],
        ChatScript::TokensAfterDelay {
            tokens: vec!["late reply".into()],
            delay_ms: 2_000,
        },
    )
    .await;
    let (state, _vault_dir) = test_state_with_ollama(&mock.url, 1);
    let state = with_mcp_token(state, TOKEN);
    let app = build_router(state);
    let session = handshake(&app).await;

    let created = tool_json(
        &call_tool(
            &app,
            &session,
            "create_idea",
            json!({ "title": "Cancel Me" }),
        )
        .await,
    );
    let slug = created["slug"].as_str().unwrap().to_string();

    let enqueue = json!({
        "jsonrpc": "2.0", "id": 60, "method": "tools/call",
        "params": { "name": "chat", "arguments": { "slug": slug, "message": "hi" }, "task": {} }
    });
    let (_, _, body) = send(&app, mcp_request(enqueue, Some(&session), Some(TOKEN))).await;
    let task_id = body["result"]["task"]["taskId"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(body["result"]["task"]["status"], "working");

    // Cancel while the (2s-delayed) job is still running.
    let cancel = json!({
        "jsonrpc": "2.0", "id": 61, "method": "tasks/cancel",
        "params": { "taskId": task_id }
    });
    let (_, _, body) = send(&app, mcp_request(cancel, Some(&session), Some(TOKEN))).await;
    assert_eq!(
        body["result"]["status"], "cancelled",
        "tasks/cancel response: {body}"
    );

    let get = json!({
        "jsonrpc": "2.0", "id": 62, "method": "tasks/get",
        "params": { "taskId": task_id }
    });
    let (_, _, body) = send(&app, mcp_request(get, Some(&session), Some(TOKEN))).await;
    assert_eq!(
        body["result"]["status"], "cancelled",
        "tasks/get after cancel must still say cancelled, not completed: {body}"
    );

    let result_req = json!({
        "jsonrpc": "2.0", "id": 63, "method": "tasks/result",
        "params": { "taskId": task_id }
    });
    let (_, _, body) = send(&app, mcp_request(result_req, Some(&session), Some(TOKEN))).await;
    assert_eq!(
        body["result"]["isError"], true,
        "a cancelled task must not report a false success: {body}"
    );
}
