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
async fn tools_list_includes_the_catalog() {
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
        "list_skills",
        "run_skill",
        "run_swarm",
        "get_artifact",
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

/// Count `## <role>` turn headings in a conversation file — the on-disk assertion every plain-call
/// test below needs (markdown-is-truth: the MCP response alone proves nothing durable).
fn count_turns(conversation: &str, role: &str) -> usize {
    conversation
        .lines()
        .filter(|l| *l == format!("## {role}"))
        .count()
}

/// ADR-0028: a Task-unaware client calls `chat` plainly; a fast model finishes inside the bounded
/// wait and the reply comes back in the `CallToolResult` itself, exactly like a synchronous tool.
#[tokio::test]
async fn plain_chat_call_returns_the_reply_when_the_model_finishes_within_the_wait() {
    let mock = spawn(
        &["llama3.2"],
        ChatScript::Tokens(vec!["a quick foil reply".into()]),
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
            json!({ "title": "Plain Chat" }),
        )
        .await,
    );
    let slug = created["slug"].as_str().unwrap().to_string();

    let result = call_tool(
        &app,
        &session,
        "chat",
        json!({ "slug": slug, "message": "steelman this" }),
    )
    .await;
    assert_ne!(
        result["isError"], true,
        "plain chat must not be a tool error: {result}"
    );
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    assert!(
        text.contains("a quick foil reply"),
        "expected the foil reply in the plain-call result: {result}"
    );

    let conversation =
        std::fs::read_to_string(vault_dir.join(&slug).join("conversation.md")).unwrap();
    assert_eq!(count_turns(&conversation, "user"), 1, "{conversation}");
    assert_eq!(count_turns(&conversation, "assistant"), 1, "{conversation}");
    assert!(conversation.contains("a quick foil reply"));
}

/// ADR-0028: when the model outlives the bounded wait, the plain call returns a non-error
/// "still running" note (the job keeps running detached, ADR-0010), and a plain retry with the
/// same arguments reattaches to that job and serves its result once — no duplicate turn.
#[tokio::test]
async fn plain_chat_call_beyond_the_wait_says_still_running_and_a_retry_picks_up_the_result() {
    let mock = spawn(
        &["llama3.2"],
        ChatScript::TokensAfterDelay {
            tokens: vec!["a slow foil reply".into()],
            delay_ms: 4_500,
        },
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
            json!({ "title": "Slow Chat" }),
        )
        .await,
    );
    let slug = created["slug"].as_str().unwrap().to_string();
    let args = json!({ "slug": slug, "message": "take your time" });

    let first = call_tool(&app, &session, "chat", args.clone()).await;
    assert_ne!(
        first["isError"], true,
        "a still-running job must not be reported as an error: {first}"
    );
    let first_text = first["content"][0]["text"].as_str().unwrap_or_default();
    assert!(
        first_text.contains("still running"),
        "expected the still-running note: {first}"
    );
    assert!(
        !first_text.contains("a slow foil reply"),
        "the reply cannot have arrived yet: {first}"
    );

    // The retry's own bounded wait outlasts the remaining mock delay, so it sees the result.
    let mut reply_text = String::new();
    for _ in 0..10 {
        let again = call_tool(&app, &session, "chat", args.clone()).await;
        assert_ne!(again["isError"], true, "{again}");
        reply_text = again["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if reply_text.contains("a slow foil reply") {
            break;
        }
    }
    assert!(
        reply_text.contains("a slow foil reply"),
        "the retry never surfaced the finished reply: {reply_text}"
    );

    let conversation =
        std::fs::read_to_string(vault_dir.join(&slug).join("conversation.md")).unwrap();
    assert_eq!(
        count_turns(&conversation, "user"),
        1,
        "the retry must not append a second user turn: {conversation}"
    );
    assert_eq!(
        count_turns(&conversation, "assistant"),
        1,
        "exactly one assistant reply must land: {conversation}"
    );
    assert_eq!(mock.chat_bodies().len(), 1, "exactly one model call");
}

/// ADR-0028: the common real-world retry — the job finished *between* the still-running note and
/// the retry. The retry must reattach to the finished task and serve its cached result, not claim
/// a second job (which would append a duplicate user turn and fire a second model call).
#[tokio::test]
async fn plain_chat_retry_after_the_job_finished_serves_the_result_without_a_second_turn() {
    let mock = spawn(
        &["llama3.2"],
        ChatScript::TokensAfterDelay {
            tokens: vec!["the finished reply".into()],
            delay_ms: 3_500,
        },
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
            json!({ "title": "Late Retry" }),
        )
        .await,
    );
    let slug = created["slug"].as_str().unwrap().to_string();
    let args = json!({ "slug": slug, "message": "finish without me" });

    let first = call_tool(&app, &session, "chat", args.clone()).await;
    let first_text = first["content"][0]["text"].as_str().unwrap_or_default();
    assert!(
        first_text.contains("still running"),
        "expected the still-running note: {first}"
    );

    // Let the job finish (3.5s mock delay vs the 3s wait) before retrying.
    tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;

    let again = call_tool(&app, &session, "chat", args).await;
    assert_ne!(again["isError"], true, "{again}");
    let text = again["content"][0]["text"].as_str().unwrap_or_default();
    assert!(
        text.contains("the finished reply"),
        "the retry must serve the finished reply, not a note or a fresh call: {again}"
    );

    let conversation =
        std::fs::read_to_string(vault_dir.join(&slug).join("conversation.md")).unwrap();
    assert_eq!(
        count_turns(&conversation, "user"),
        1,
        "the retry must not append a duplicate user turn: {conversation}"
    );
    assert_eq!(count_turns(&conversation, "assistant"), 1, "{conversation}");
    assert_eq!(mock.chat_bodies().len(), 1, "exactly one model call");
}

/// ADR-0028: a plain `chat` with a *different* message while the previous one is still running
/// is a genuinely new operation on a busy idea — an honest "already busy" error, exactly like
/// task mode; the new message must neither be swallowed nor persisted.
#[tokio::test]
async fn plain_chat_with_a_different_message_while_busy_is_an_already_busy_error() {
    let mock = spawn(
        &["llama3.2"],
        ChatScript::TokensAfterDelay {
            tokens: vec!["reply to the first".into()],
            delay_ms: 3_500,
        },
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
            json!({ "title": "Busy Idea" }),
        )
        .await,
    );
    let slug = created["slug"].as_str().unwrap().to_string();

    let first = call_tool(
        &app,
        &session,
        "chat",
        json!({ "slug": slug, "message": "the first message" }),
    )
    .await;
    assert!(
        first["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("still running"),
        "{first}"
    );

    let req = json!({
        "jsonrpc": "2.0", "id": 80, "method": "tools/call",
        "params": { "name": "chat", "arguments": { "slug": slug, "message": "a second message" } }
    });
    let (_, _, body) = send(&app, mcp_request(req, Some(&session), Some(TOKEN))).await;
    let err = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        err.contains("already busy"),
        "a new message on a busy idea must be an already-busy error, not a swallowed reply: {body}"
    );

    let conversation =
        std::fs::read_to_string(vault_dir.join(&slug).join("conversation.md")).unwrap();
    assert!(conversation.contains("the first message"), "{conversation}");
    assert!(
        !conversation.contains("a second message"),
        "the rejected message must not be persisted: {conversation}"
    );
    assert_eq!(count_turns(&conversation, "user"), 1, "{conversation}");
}

/// ADR-0028: a plain `chat` retry while a task-mode `chat` for the same idea is still `Working`
/// reattaches to that in-flight task instead of erroring "already busy" or spawning a second job.
#[tokio::test]
async fn plain_chat_retry_reattaches_to_an_in_flight_task_for_the_same_idea() {
    let mock = spawn(
        &["llama3.2"],
        ChatScript::TokensAfterDelay {
            tokens: vec!["the one reply".into()],
            delay_ms: 1_500,
        },
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
            json!({ "title": "Reattach Me" }),
        )
        .await,
    );
    let slug = created["slug"].as_str().unwrap().to_string();

    let enqueue = json!({
        "jsonrpc": "2.0", "id": 70, "method": "tools/call",
        "params": { "name": "chat", "arguments": { "slug": slug, "message": "hi" }, "task": {} }
    });
    let (status, _, body) = send(&app, mcp_request(enqueue, Some(&session), Some(TOKEN))).await;
    assert_eq!(status, StatusCode::OK, "enqueue_task failed: {body}");
    assert_eq!(body["result"]["task"]["status"], "working");
    let task_id = body["result"]["task"]["taskId"]
        .as_str()
        .unwrap()
        .to_string();

    // Still Working (1.5s mock delay): the plain retry must neither error nor spawn a second job.
    let req = json!({
        "jsonrpc": "2.0", "id": 71, "method": "tools/call",
        "params": { "name": "chat", "arguments": { "slug": slug, "message": "hi" } }
    });
    let (status, _, body) = send(&app, mcp_request(req, Some(&session), Some(TOKEN))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.get("error").is_none(),
        "a plain retry on a busy idea must reattach, not error: {body}"
    );
    let text = body["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        text.contains("the one reply"),
        "the retry should have waited out the in-flight task: {body}"
    );

    // The task-mode side still sees the very same task as completed (shared terminal cache).
    let get = json!({
        "jsonrpc": "2.0", "id": 72, "method": "tasks/get",
        "params": { "taskId": task_id }
    });
    let (_, _, body) = send(&app, mcp_request(get, Some(&session), Some(TOKEN))).await;
    assert_eq!(body["result"]["status"], "completed", "{body}");

    let conversation =
        std::fs::read_to_string(vault_dir.join(&slug).join("conversation.md")).unwrap();
    assert_eq!(count_turns(&conversation, "user"), 1, "{conversation}");
    assert_eq!(count_turns(&conversation, "assistant"), 1, "{conversation}");
    assert_eq!(mock.chat_bodies().len(), 1, "exactly one model call");
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

/// A full `tools/call` JSON-RPC response — for the long-running tools, whose business errors are
/// protocol-level `invalid_params` (docs/adr/0024) and so live under `error`, not `result`.
async fn call_tool_body(app: &Router, session: &str, name: &str, args: Value) -> Value {
    let req = json!({
        "jsonrpc": "2.0", "id": 90, "method": "tools/call",
        "params": { "name": name, "arguments": args }
    });
    let (status, _, body) = send(app, mcp_request(req, Some(session), Some(TOKEN))).await;
    assert_eq!(status, StatusCode::OK, "tools/call {name} failed: {body}");
    body
}

/// Create an idea over MCP and move it into discussion with one owner turn on disk — the D9
/// precondition for moves and store, without spending a chat round-trip on it.
async fn create_in_discussion(app: &Router, session: &str, vault_dir: &std::path::Path) -> String {
    let created =
        tool_json(&call_tool(app, session, "create_idea", json!({ "title": "Moves" })).await);
    let slug = created["slug"].as_str().unwrap().to_string();
    set_state(
        vault_dir,
        &slug,
        idea_vault::domain::IdeaState::InDiscussion,
    );
    idea_vault::vault::store::append_turn(vault_dir, &slug, "user", "the idea in full").unwrap();
    slug
}

fn set_state(vault_dir: &std::path::Path, slug: &str, state: idea_vault::domain::IdeaState) {
    let mut idea = idea_vault::vault::store::read_idea(vault_dir, slug).unwrap();
    idea.frontmatter.state = state;
    idea_vault::vault::store::write_idea(vault_dir, &idea).unwrap();
}

/// Plain-call a long-running tool until it stops answering "still running" (ADR-0028 retry).
async fn call_until_done(app: &Router, session: &str, name: &str, args: Value) -> Value {
    for _ in 0..20 {
        let result = call_tool(app, session, name, args.clone()).await;
        let text = result["content"][0]["text"].as_str().unwrap_or_default();
        if !text.contains("is still running") {
            return result;
        }
    }
    panic!("{name} never finished");
}

#[tokio::test]
async fn list_skills_returns_the_skill_book_with_stages() {
    let (state, _vault) = test_state();
    let state = with_mcp_token(state, TOKEN);
    let app = build_router(state);
    let session = handshake(&app).await;

    let skills = tool_json(&call_tool(&app, &session, "list_skills", json!({})).await);
    let skills = skills.as_array().expect("skills array");
    let premortem = skills
        .iter()
        .find(|s| s["name"] == "premortem")
        .unwrap_or_else(|| panic!("premortem missing: {skills:?}"));
    assert_eq!(premortem["stage"], "attack");
    assert_eq!(premortem["source"], "built-in");
    // Hidden extract-* lenses are knowledge-extraction internals, not moves (ADR-0015).
    assert!(skills
        .iter()
        .all(|s| !s["name"].as_str().unwrap().starts_with("extract-")));
}

#[tokio::test]
async fn run_skill_plain_call_appends_one_assistant_turn_and_returns_it() {
    let mock = spawn(
        &["llama3.2"],
        ChatScript::Tokens(vec!["1. ranked cause".into()]),
    )
    .await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    let state = with_mcp_token(state, TOKEN);
    let app = build_router(state);
    let session = handshake(&app).await;
    let slug = create_in_discussion(&app, &session, &vault_dir).await;

    let result = call_until_done(
        &app,
        &session,
        "run_skill",
        json!({ "slug": slug, "name": "premortem" }),
    )
    .await;
    assert_ne!(result["isError"], true, "{result}");
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("ranked cause"), "{result}");

    let conversation =
        std::fs::read_to_string(vault_dir.join(&slug).join("conversation.md")).unwrap();
    assert_eq!(
        conversation.matches("## assistant").count(),
        1,
        "{conversation}"
    );
}

#[tokio::test]
async fn run_skill_refuses_unknown_skill_draft_stored_and_busy_ideas() {
    let mock = spawn(
        &["llama3.2"],
        ChatScript::TokensAfterDelay {
            tokens: vec!["slow move".into()],
            delay_ms: 2_000,
        },
    )
    .await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    let state = with_mcp_token(state, TOKEN);
    let app = build_router(state);
    let session = handshake(&app).await;
    let slug = create_in_discussion(&app, &session, &vault_dir).await;

    let body = call_tool_body(
        &app,
        &session,
        "run_skill",
        json!({ "slug": slug, "name": "nope" }),
    )
    .await;
    assert!(body["error"].is_object(), "unknown skill: {body}");

    for state in [
        idea_vault::domain::IdeaState::Draft,
        idea_vault::domain::IdeaState::Stored,
    ] {
        set_state(&vault_dir, &slug, state);
        let body = call_tool_body(
            &app,
            &session,
            "run_skill",
            json!({ "slug": slug, "name": "premortem" }),
        )
        .await;
        assert!(body["error"].is_object(), "{state:?} must refuse: {body}");
    }
    assert!(
        mock.chat_bodies().is_empty(),
        "no guard failure may reach the model"
    );

    // Busy: a running move holds the claim; a different move is refused, not queued.
    set_state(
        &vault_dir,
        &slug,
        idea_vault::domain::IdeaState::InDiscussion,
    );
    let first = call_tool(
        &app,
        &session,
        "run_skill",
        json!({ "slug": slug, "name": "premortem" }),
    )
    .await;
    assert!(
        first["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("is still running"),
        "{first}"
    );
    let body = call_tool_body(
        &app,
        &session,
        "run_skill",
        json!({ "slug": slug, "name": "cheapest-disproof" }),
    )
    .await;
    assert!(body["error"].is_object(), "busy idea must refuse: {body}");
}

#[tokio::test]
async fn run_swarm_as_a_task_persists_exactly_one_synthesis_turn() {
    let mock = spawn(
        &["llama3.2"],
        ChatScript::Tokens(vec!["converged finding".into()]),
    )
    .await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    let state = with_mcp_token(state, TOKEN);
    let app = build_router(state);
    let session = handshake(&app).await;
    let slug = create_in_discussion(&app, &session, &vault_dir).await;

    let enqueue = json!({
        "jsonrpc": "2.0", "id": 80, "method": "tools/call",
        "params": { "name": "run_swarm", "arguments": { "slug": slug }, "task": {} }
    });
    let (_, _, body) = send(&app, mcp_request(enqueue, Some(&session), Some(TOKEN))).await;
    let task_id = body["result"]["task"]["taskId"]
        .as_str()
        .unwrap_or_else(|| panic!("no taskId: {body}"))
        .to_string();

    let mut status_str = "working".to_string();
    for _ in 0..500 {
        let get = json!({
            "jsonrpc": "2.0", "id": 81, "method": "tasks/get",
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
    assert_eq!(status_str, "completed");

    let result_req = json!({
        "jsonrpc": "2.0", "id": 82, "method": "tasks/result",
        "params": { "taskId": task_id }
    });
    let (_, _, body) = send(&app, mcp_request(result_req, Some(&session), Some(TOKEN))).await;
    let text = body["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(text.contains("converged finding"), "{body}");

    let conversation =
        std::fs::read_to_string(vault_dir.join(&slug).join("conversation.md")).unwrap();
    assert_eq!(
        conversation.matches("## assistant (swarm: ").count(),
        1,
        "{conversation}"
    );
    assert!(
        mock.chat_bodies().len() > 1,
        "a swarm fans out more than one call"
    );
}

#[tokio::test]
async fn run_swarm_rejects_oversized_unknown_and_capstone_angles_before_any_model_call() {
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec!["x".into()])).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    let state = with_mcp_token(state, TOKEN);
    let app = build_router(state);
    let session = handshake(&app).await;
    let slug = create_in_discussion(&app, &session, &vault_dir).await;

    let nine: Vec<&str> = std::iter::repeat_n("premortem", 9).collect();
    for angles in [
        json!(nine),
        json!(["nope"]),
        json!(["build-prompt"]),
        json!("premortem"),
    ] {
        let body = call_tool_body(
            &app,
            &session,
            "run_swarm",
            json!({ "slug": slug, "angles": angles }),
        )
        .await;
        assert!(
            body["error"].is_object(),
            "angles {angles} must refuse: {body}"
        );
    }
    assert!(mock.chat_bodies().is_empty());
}

#[tokio::test]
async fn get_idea_after_store_exposes_fact_bodies_and_the_quarantine_artifact() {
    let mock = support::spawn_sequence(
        &["llama3.2"],
        vec![
            ChatScript::Tokens(vec!["## Consolidated\n\nthe idea in full".into()]),
            ChatScript::Tokens(vec![
                "FACT: Core claim\nQUOTE: \"the idea in full\"\nThe grounded body.\n\
                 FACT: Invented\nQUOTE: \"we raise a seed round\"\nNever said.\n"
                    .into(),
            ]),
        ],
    )
    .await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    let state = with_mcp_token(state, TOKEN);
    let app = build_router(state);
    let session = handshake(&app).await;
    let slug = create_in_discussion(&app, &session, &vault_dir).await;

    let stored = call_until_done(&app, &session, "store_idea", json!({ "slug": slug })).await;
    let text = stored["content"][0]["text"].as_str().unwrap_or_default();
    assert!(text.starts_with("stored"), "{stored}");

    let idea = tool_json(&call_tool(&app, &session, "get_idea", json!({ "slug": slug })).await);
    assert_eq!(idea["state"], "stored");
    let memory = idea["memory"].as_array().unwrap();
    assert_eq!(
        memory.len(),
        1,
        "only the grounded fact is remembered: {memory:?}"
    );
    assert_eq!(memory[0]["slug"], "core-claim");
    assert!(memory[0]["body"]
        .as_str()
        .unwrap()
        .contains("The grounded body."));
    let quarantine = idea["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["kind"] == "quarantine")
        .unwrap_or_else(|| panic!("no quarantine artifact: {idea}"))
        .clone();

    let artifact = tool_json(
        &call_tool(
            &app,
            &session,
            "get_artifact",
            json!({ "slug": slug, "artifact": quarantine["slug"] }),
        )
        .await,
    );
    assert!(artifact["body"]
        .as_str()
        .unwrap()
        .contains("we raise a seed round"));

    let missing = call_tool(
        &app,
        &session,
        "get_artifact",
        json!({ "slug": slug, "artifact": "no-such-artifact" }),
    )
    .await;
    assert_eq!(missing["isError"], true, "{missing}");
}

#[tokio::test]
async fn store_idea_refuses_an_in_discussion_idea_with_no_turns() {
    let (state, vault_dir) = test_state();
    let state = with_mcp_token(state, TOKEN);
    let app = build_router(state);
    let session = handshake(&app).await;
    let created =
        tool_json(&call_tool(&app, &session, "create_idea", json!({ "title": "Empty" })).await);
    let slug = created["slug"].as_str().unwrap().to_string();
    set_state(
        &vault_dir,
        &slug,
        idea_vault::domain::IdeaState::InDiscussion,
    );

    let body = call_tool_body(&app, &session, "store_idea", json!({ "slug": slug })).await;
    assert!(body["error"].is_object(), "{body}");
}

#[tokio::test]
async fn search_and_reopen_idea_work_over_mcp() {
    let (state, vault_dir) = test_state();
    let state = with_mcp_token(state, TOKEN);
    let app = build_router(state);
    let session = handshake(&app).await;
    let created = tool_json(
        &call_tool(
            &app,
            &session,
            "create_idea",
            json!({ "title": "Searchable", "body": "a zeppelin courier service" }),
        )
        .await,
    );
    let slug = created["slug"].as_str().unwrap().to_string();

    let hits =
        tool_json(&call_tool(&app, &session, "search", json!({ "query": "zeppelin" })).await);
    assert!(
        hits.as_array().unwrap().iter().any(|h| h["slug"] == slug),
        "{hits}"
    );

    // Reopen is only an edge from Stored (D9).
    let refused = call_tool(&app, &session, "reopen_idea", json!({ "slug": slug })).await;
    assert_eq!(refused["isError"], true, "{refused}");
    set_state(&vault_dir, &slug, idea_vault::domain::IdeaState::Stored);
    let reopened =
        tool_json(&call_tool(&app, &session, "reopen_idea", json!({ "slug": slug })).await);
    assert_eq!(reopened["state"], "reopened");
}

// ---- Idempotent replay (docs/adr/0033, amending ADR-0028's forget-after-serve) ----

const REPLAY_PREFIX: &str = "(replayed result of task ";

fn text_of(result: &Value) -> String {
    result["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn conversation_of(vault_dir: &std::path::Path, slug: &str) -> String {
    std::fs::read_to_string(vault_dir.join(slug).join("conversation.md")).unwrap()
}

/// Poll `tasks/get` until the task leaves `working`, returning the final status.
async fn wait_task(app: &Router, session: &str, task_id: &str) -> String {
    for _ in 0..500 {
        let get = json!({
            "jsonrpc": "2.0", "id": 101, "method": "tasks/get",
            "params": { "taskId": task_id }
        });
        let (_, _, body) = send(app, mcp_request(get, Some(session), Some(TOKEN))).await;
        let status = body["result"]["status"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if status != "working" {
            return status;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("task {task_id} never left working");
}

async fn task_result(app: &Router, session: &str, task_id: &str) -> Value {
    let req = json!({
        "jsonrpc": "2.0", "id": 102, "method": "tasks/result",
        "params": { "taskId": task_id }
    });
    let (_, _, body) = send(app, mcp_request(req, Some(session), Some(TOKEN))).await;
    body["result"].clone()
}

/// A planner answer the quick build-plan finish accepts (goal, a settled quote from the owner's
/// turn, one runnable task) — the same shape `tests/build_plan_flow.rs` uses.
const PLANNER_ANSWER: &str = "## Goal
Disprove the strategy cheaply before building.

## Settled
- S1: Disproof comes before any code.
  quote: \"the cheapest disproof before any Rust exists\"

## Verify first
- none

## Open questions
- Q1: Which market do we backtest first?

## Plan
- [ ] T1: Write the spec with a dated kill criterion
  touches: `SPEC.md`
  accept: `test -s SPEC.md` → exit 0

## Kill criteria
- none";

fn build_plans(vault_dir: &std::path::Path, slug: &str) -> usize {
    idea_vault::vault::store::read_artifacts(vault_dir, slug)
        .unwrap()
        .into_iter()
        .filter(|a| a.frontmatter.kind == idea_vault::domain::ArtifactKind::BuildPlan)
        .count()
}

/// The owner's complaint behind ADR-0033: a plain `run_skill build-prompt` retried after its
/// result was served used to start a second run and write a second, unrelated plan.
#[tokio::test]
async fn plain_run_skill_build_prompt_retry_after_served_replays_without_second_plan() {
    let mock = spawn(
        &["llama3.2"],
        ChatScript::Tokens(vec![PLANNER_ANSWER.into()]),
    )
    .await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    let state = with_mcp_token(state, TOKEN);
    let app = build_router(state);
    let session = handshake(&app).await;
    let slug = create_in_discussion(&app, &session, &vault_dir).await;
    idea_vault::vault::store::append_turn(
        &vault_dir,
        &slug,
        "user",
        "We run the cheapest disproof before any Rust exists.",
    )
    .unwrap();
    let args = json!({ "slug": slug, "name": "build-prompt" });

    let first = call_until_done(&app, &session, "run_skill", args.clone()).await;
    assert_ne!(first["isError"], true, "{first}");
    assert_eq!(build_plans(&vault_dir, &slug), 1);
    assert_eq!(
        mock.chat_bodies().len(),
        1,
        "a quick plan is one model call"
    );
    let turns = conversation_of(&vault_dir, &slug);

    let again = call_until_done(&app, &session, "run_skill", args).await;
    assert_ne!(again["isError"], true, "{again}");
    let (first_text, again_text) = (text_of(&first), text_of(&again));
    assert!(
        again_text.starts_with(REPLAY_PREFIX) && again_text.ends_with(&first_text),
        "the retry must replay the served result verbatim: {again_text}"
    );
    assert_eq!(build_plans(&vault_dir, &slug), 1, "no second plan");
    assert_eq!(mock.chat_bodies().len(), 1, "no second model call");
    assert_eq!(conversation_of(&vault_dir, &slug), turns, "no new turn");
}

/// The args-hash replay is a retry guess, bounded by the turn count: the same message after an
/// intervening turn is a new question and runs again.
#[tokio::test]
async fn identical_chat_after_intervening_turn_starts_new_run() {
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec!["foil reply".into()])).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    let state = with_mcp_token(state, TOKEN);
    let app = build_router(state);
    let session = handshake(&app).await;
    let slug = create_in_discussion(&app, &session, &vault_dir).await;
    let args = json!({ "slug": slug, "message": "and what about cost?" });

    let first = call_until_done(&app, &session, "chat", args.clone()).await;
    assert!(!text_of(&first).starts_with(REPLAY_PREFIX), "{first}");
    let replay = call_until_done(&app, &session, "chat", args.clone()).await;
    assert!(
        text_of(&replay).starts_with(REPLAY_PREFIX),
        "no turn in between: {replay}"
    );
    assert_eq!(mock.chat_bodies().len(), 1);

    idea_vault::vault::store::append_turn(&vault_dir, &slug, "user", "an intervening thought")
        .unwrap();
    let rerun = call_until_done(&app, &session, "chat", args).await;
    assert_ne!(rerun["isError"], true, "{rerun}");
    assert!(
        !text_of(&rerun).starts_with(REPLAY_PREFIX),
        "a turn landed since: {rerun}"
    );
    assert_eq!(
        mock.chat_bodies().len(),
        2,
        "the new question reached the model"
    );
    let conversation = conversation_of(&vault_dir, &slug);
    assert_eq!(
        conversation.matches("and what about cost?").count(),
        2,
        "{conversation}"
    );
}

#[tokio::test]
async fn idempotency_key_with_different_args_is_invalid_params() {
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec!["foil reply".into()])).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    let state = with_mcp_token(state, TOKEN);
    let app = build_router(state);
    let session = handshake(&app).await;
    let slug = create_in_discussion(&app, &session, &vault_dir).await;

    let first = call_until_done(
        &app,
        &session,
        "chat",
        json!({ "slug": slug, "message": "first words", "idempotency_key": "k1" }),
    )
    .await;
    assert_ne!(first["isError"], true, "{first}");

    let body = call_tool_body(
        &app,
        &session,
        "chat",
        json!({ "slug": slug, "message": "other words", "idempotency_key": "k1" }),
    )
    .await;
    let err = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        err.contains("idempotency_key reused with different arguments"),
        "{body}"
    );
    assert!(!conversation_of(&vault_dir, &slug).contains("other words"));
    assert_eq!(mock.chat_bodies().len(), 1);
}

/// An explicit key is the client naming the operation, so it replays past later turns.
#[tokio::test]
async fn idempotency_key_same_args_replays_after_intervening_turn() {
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec!["foil reply".into()])).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    let state = with_mcp_token(state, TOKEN);
    let app = build_router(state);
    let session = handshake(&app).await;
    let slug = create_in_discussion(&app, &session, &vault_dir).await;
    let args = json!({ "slug": slug, "message": "keyed words", "idempotency_key": "k2" });

    let first = call_until_done(&app, &session, "chat", args.clone()).await;
    assert_ne!(first["isError"], true, "{first}");
    idea_vault::vault::store::append_turn(&vault_dir, &slug, "user", "an intervening thought")
        .unwrap();
    let again = call_until_done(&app, &session, "chat", args).await;
    assert_ne!(again["isError"], true, "{again}");
    let again_text = text_of(&again);
    assert!(
        again_text.starts_with(REPLAY_PREFIX) && again_text.ends_with(&text_of(&first)),
        "{again}"
    );
    assert_eq!(mock.chat_bodies().len(), 1);
    assert_eq!(
        conversation_of(&vault_dir, &slug)
            .matches("keyed words")
            .count(),
        1
    );
}

#[tokio::test]
async fn task_mode_replay_hit_is_terminal_immediately() {
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec!["foil reply".into()])).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    let state = with_mcp_token(state, TOKEN);
    let app = build_router(state);
    let session = handshake(&app).await;
    let slug = create_in_discussion(&app, &session, &vault_dir).await;
    let args = json!({ "slug": slug, "message": "once only" });

    let first = call_until_done(&app, &session, "chat", args.clone()).await;
    assert_ne!(first["isError"], true, "{first}");

    let enqueue = json!({
        "jsonrpc": "2.0", "id": 103, "method": "tools/call",
        "params": { "name": "chat", "arguments": args, "task": {} }
    });
    let (status, _, body) = send(&app, mcp_request(enqueue, Some(&session), Some(TOKEN))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["result"]["task"]["status"], "completed",
        "a replay hit is terminal from birth: {body}"
    );
    let task_id = body["result"]["task"]["taskId"].as_str().unwrap();
    let result = task_result(&app, &session, task_id).await;
    let text = text_of(&result);
    assert!(
        text.starts_with(REPLAY_PREFIX) && text.ends_with(&text_of(&first)),
        "{result}"
    );
    assert_eq!(mock.chat_bodies().len(), 1);
    assert_eq!(
        count_turns(&conversation_of(&vault_dir, &slug), "user"),
        2,
        "the seed turn and the one chat turn"
    );
}

#[tokio::test]
async fn failed_run_is_not_cached_and_retry_runs_again() {
    let mock = support::spawn_sequence(
        &["llama3.2"],
        vec![
            ChatScript::EofAfter(vec!["partial".into()]),
            ChatScript::Tokens(vec!["second try".into()]),
        ],
    )
    .await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    let state = with_mcp_token(state, TOKEN);
    let app = build_router(state);
    let session = handshake(&app).await;
    let slug = create_in_discussion(&app, &session, &vault_dir).await;
    let args = json!({ "slug": slug, "message": "try me" });

    let failed = call_until_done(&app, &session, "chat", args.clone()).await;
    assert_eq!(failed["isError"], true, "{failed}");

    let retry = call_until_done(&app, &session, "chat", args).await;
    assert_ne!(retry["isError"], true, "{retry}");
    let text = text_of(&retry);
    assert!(
        !text.starts_with(REPLAY_PREFIX) && text.contains("second try"),
        "{retry}"
    );
    assert_eq!(mock.chat_bodies().len(), 2, "the retry reached the model");
}

/// Render-once: a task's result is fixed when it first turns terminal, so a later turn cannot
/// be served as this task's reply.
#[tokio::test]
async fn tasks_result_after_later_turn_returns_own_reply() {
    let mock = spawn(&["llama3.2"], ChatScript::Tokens(vec!["own reply".into()])).await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    let state = with_mcp_token(state, TOKEN);
    let app = build_router(state);
    let session = handshake(&app).await;
    let slug = create_in_discussion(&app, &session, &vault_dir).await;

    let enqueue = json!({
        "jsonrpc": "2.0", "id": 104, "method": "tools/call",
        "params": { "name": "chat", "arguments": { "slug": slug, "message": "hi" }, "task": {} }
    });
    let (_, _, body) = send(&app, mcp_request(enqueue, Some(&session), Some(TOKEN))).await;
    let task_id = body["result"]["task"]["taskId"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(wait_task(&app, &session, &task_id).await, "completed");

    idea_vault::vault::store::append_turn(&vault_dir, &slug, "assistant", "a later turn").unwrap();
    let result = task_result(&app, &session, &task_id).await;
    let text = text_of(&result);
    assert!(
        text.contains("own reply") && !text.contains("a later turn"),
        "{result}"
    );
}

#[tokio::test]
async fn store_idea_retry_after_served_replays() {
    let mock = spawn_store_ready_mock().await;
    let (state, vault_dir) = test_state_with_ollama(&mock.url, 1);
    let state = with_mcp_token(state, TOKEN);
    let app = build_router(state);
    let session = handshake(&app).await;
    let slug = create_in_discussion(&app, &session, &vault_dir).await;
    let args = json!({ "slug": slug });

    let stored = call_until_done(&app, &session, "store_idea", args.clone()).await;
    assert!(text_of(&stored).starts_with("stored"), "{stored}");
    let calls = mock.chat_bodies().len();

    let again = call_until_done(&app, &session, "store_idea", args).await;
    assert_ne!(
        again["isError"], true,
        "a served store must replay, not refuse the now-Stored idea: {again}"
    );
    let again_text = text_of(&again);
    assert!(
        again_text.starts_with(REPLAY_PREFIX) && again_text.ends_with(&text_of(&stored)),
        "{again}"
    );
    assert_eq!(mock.chat_bodies().len(), calls, "no second store run");
}
