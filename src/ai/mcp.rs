//! MCP (Model Context Protocol) **Streamable-HTTP** client — a second, orthogonal way for the
//! foil to gain tools, alongside `ai::web`'s hardcoded search/fetch leaves. An MCP server exposes
//! an arbitrary tool set (filesystem, project trackers, whatever the owner points it at) behind
//! one JSON-RPC endpoint; this module is the wire client for that protocol, nothing more — it
//! does not decide *which* tools get exposed to a model or how results feed a prompt. Which
//! servers exist (and are enabled) lives in the persistent [`crate::mcp`] registry, and
//! `ai::backend`'s tool loop is the bridge that combines the two — this module must never import
//! `crate::mcp` (nor vice versa); the one-way `backend → {ai::mcp, crate::mcp}` edges keep the
//! graph acyclic.
//!
//! Verified against the Streamable-HTTP transport as implemented by `rmcp` 1.8.0
//! (docs/05-ai-integration.md's "local only" boundary is unaffected: the MCP server URL is owner
//! config, not a hardcoded cloud endpoint, and is never assumed to be `localhost` — it is passed
//! in explicitly, matching `ai::ollama`'s discipline).
//!
//! Wire shape, condensed:
//! - One POST per JSON-RPC call. Every POST carries `Accept: application/json, text/event-stream`
//!   (rmcp 406s without both substrings present) and `Content-Type: application/json`; an
//!   `Authorization: Bearer <token>` header is added only when the caller configured one.
//! - `initialize` is the first call and never carries `Mcp-Session-Id`; every subsequent call
//!   does, **if** the server handed one back (stateless servers may not — both are tolerated).
//! - The server may answer with a plain `application/json` body OR (rmcp's default) with
//!   `text/event-stream` carrying exactly one JSON-RPC message per response, unrelated to whether
//!   the *client* asked to stream. [`parse_rpc_response`] parses both shapes.
//! - `notifications/initialized` is optional in rmcp 1.8 and deliberately skipped here — one round
//!   trip fewer, no observed server that requires it.
//!
//! Degrade discipline (mirrors `ai::web::execute_tool`): every failure mode this module can hit —
//! transport error, timeout, HTTP 401/404/406/415, a JSON-RPC `error` object, or a tool call that
//! completed with `isError: true` — comes back as a readable `Err(String)` (or, for `isError`, a
//! prefixed `Ok` string), never a panic. The one exception the protocol itself defines: a `404` on
//! any POST other than `initialize` means the session expired (or the server is stateless and
//! never had one) — [`McpSession`] re-initializes once and retries the call once before giving up.

use std::time::Duration;

use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use serde_json::{json, Value};

/// Per-request wall clock (connect + send + full body read). Generous relative to a local Ollama
/// call because a tool round trip is one-shot request/response, not a token stream.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Connect timeout: an unreachable MCP server should fail fast, not eat into the request budget.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// The MCP protocol revision this client speaks (`initialize.params.protocolVersion`).
const PROTOCOL_VERSION: &str = "2025-06-18";
/// Non-standard but protocol-mandated header carrying the server-issued session id.
const SESSION_HEADER: &str = "Mcp-Session-Id";

/// One tool advertised by an MCP server (`tools/list` result entry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    /// The tool's JSON Schema for `arguments`, passed through opaquely — this module has no
    /// opinion on tool semantics, only on the transport.
    pub input_schema: Value,
}

/// A configured MCP server endpoint. Cheap to construct per server; [`connect`](Self::connect)
/// performs the `initialize` handshake and returns a [`McpSession`] borrowed from it.
pub struct McpClient {
    url: String,
    bearer_token: Option<String>,
    http: reqwest::Client,
}

impl McpClient {
    /// The endpoint this client dials (the bridge's cache key includes it, so an edited server
    /// URL is a natural cache miss).
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Build a client for the single-endpoint MCP server at `url`. `bearer_token` is sent as
    /// `Authorization: Bearer <token>` on every POST when present; omitted entirely otherwise (an
    /// MCP server with no auth configured must never see an empty/placeholder bearer header).
    pub fn new(url: impl Into<String>, bearer_token: Option<String>) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| format!("mcp http client: {e}"))?;
        Ok(Self {
            url: url.into(),
            bearer_token,
            http,
        })
    }

    /// Perform the `initialize` handshake and return a session ready for `tools/list`/`tools/call`.
    pub async fn connect(&self) -> Result<McpSession<'_>, String> {
        let mut session = McpSession {
            client: self,
            session_id: None,
            next_id: 1,
        };
        session.initialize().await?;
        Ok(session)
    }
}

/// A live MCP conversation: the server-issued session id (if any) plus a monotonically
/// increasing JSON-RPC request id. Borrows the [`McpClient`] it was created from — one session
/// per `connect()` call, same lifetime discipline as any other request-scoped handle in this
/// crate.
pub struct McpSession<'a> {
    client: &'a McpClient,
    session_id: Option<String>,
    next_id: u64,
}

/// Outcome of one raw POST, before request-level retry policy is applied. [`NotFound`] is split
/// out from [`Other`] because it is the one status the caller (`request`) reacts to specially
/// (expired/unknown session — re-initialize once and retry).
enum PostError {
    NotFound,
    Other(String),
}

impl PostError {
    fn into_message(self) -> String {
        match self {
            PostError::NotFound => "mcp session not found (404)".to_string(),
            PostError::Other(msg) => msg,
        }
    }
}

impl McpSession<'_> {
    /// List the tools the server exposes (`tools/list`).
    pub async fn list_tools(&mut self) -> Result<Vec<McpTool>, String> {
        let result = self.request("tools/list", None).await?;
        parse_tools(&result)
    }

    /// Invoke one tool (`tools/call`) and flatten its `content[]` text blocks into one string.
    /// A tool-level failure (`isError: true`) is **not** an `Err` — it comes back `Ok` with a
    /// `"tool error: "` prefix so the caller can hand it to a model as readable content, exactly
    /// like `ai::web::execute_tool`'s errors-are-content rule.
    pub async fn call_tool(&mut self, name: &str, arguments: &Value) -> Result<String, String> {
        let params = json!({ "name": name, "arguments": arguments });
        let result = self.request("tools/call", Some(params)).await?;
        parse_call_result(&result)
    }

    /// `initialize`: never retried by [`request`] (there is nothing to fall back to), and always
    /// clears any prior session id first — a stale id must not leak onto the handshake that is
    /// meant to establish a fresh one.
    async fn initialize(&mut self) -> Result<(), String> {
        let id = self.next_id;
        self.next_id += 1;
        self.session_id = None;

        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "idea-vault", "version": env!("CARGO_PKG_VERSION") },
        });
        let (envelope, new_session) = self
            .post_once("initialize", Some(params), id)
            .await
            .map_err(PostError::into_message)?;
        self.session_id = new_session;
        extract_result_or_error(envelope)?;
        Ok(())
    }

    /// Send one JSON-RPC call and unwrap its `result`/`error`. On a `404` to anything other than
    /// `initialize`, re-initializes once and retries the same call once (fresh session, same
    /// method/params/id) before giving up — the one protocol-level retry this client performs.
    async fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;

        match self.post_once(method, params.clone(), id).await {
            Ok((envelope, new_session)) => {
                if let Some(sid) = new_session {
                    self.session_id = Some(sid);
                }
                extract_result_or_error(envelope)
            }
            Err(PostError::NotFound) => {
                self.initialize()
                    .await
                    .map_err(|e| format!("mcp session expired; re-initialize failed: {e}"))?;
                let (envelope, new_session) =
                    self.post_once(method, params, id).await.map_err(|e| {
                        format!("mcp session expired; retry failed: {}", e.into_message())
                    })?;
                if let Some(sid) = new_session {
                    self.session_id = Some(sid);
                }
                extract_result_or_error(envelope)
            }
            Err(e) => Err(e.into_message()),
        }
    }

    /// One raw POST: build the JSON-RPC envelope, attach the required headers, and parse either a
    /// plain-JSON or SSE response body into the JSON-RPC envelope. Returns the server's fresh
    /// `Mcp-Session-Id` response header alongside the envelope — the caller decides whether/how to
    /// store it (kept out of `&mut self` here so `initialize` and `request` share one code path
    /// regardless of retry state).
    async fn post_once(
        &self,
        method: &str,
        params: Option<Value>,
        id: u64,
    ) -> Result<(Value, Option<String>), PostError> {
        let mut req = self
            .client
            .http
            .post(&self.client.url)
            .header(ACCEPT, "application/json, text/event-stream")
            .header(CONTENT_TYPE, "application/json");
        if let Some(token) = &self.client.bearer_token {
            req = req.header(AUTHORIZATION, format!("Bearer {token}"));
        }
        if method != "initialize" {
            if let Some(sid) = &self.session_id {
                req = req.header(SESSION_HEADER, sid);
            }
        }

        let mut body = json!({ "jsonrpc": "2.0", "id": id, "method": method });
        if let Some(p) = params {
            body["params"] = p;
        }

        let resp = req
            .json(&body)
            .send()
            .await
            .map_err(|e| PostError::Other(format!("mcp request failed: {e}")))?;

        let status = resp.status();
        match status.as_u16() {
            401 => {
                return Err(PostError::Other(
                    "mcp server rejected credentials (401)".to_string(),
                ))
            }
            404 => return Err(PostError::NotFound),
            406 | 415 => {
                return Err(PostError::Other(format!(
                    "mcp server rejected request headers ({status})"
                )))
            }
            _ => {}
        }

        let new_session = resp
            .headers()
            .get(SESSION_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let content_type = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let text = resp
            .text()
            .await
            .map_err(|e| PostError::Other(format!("reading mcp response: {e}")))?;

        if !status.is_success() {
            return Err(PostError::Other(format!(
                "mcp server returned {status}: {text}"
            )));
        }

        let envelope = parse_rpc_response(&content_type, &text, id).map_err(PostError::Other)?;
        Ok((envelope, new_session))
    }
}

/// Parse one JSON-RPC response body, whatever shape the server chose to send it in: plain
/// `application/json`, or (rmcp's default) `text/event-stream` with the message riding in one or
/// more `data:` lines. Pure and socket-free — every branch is unit-tested directly against
/// fixtures rather than a live server.
pub(crate) fn parse_rpc_response(
    content_type: &str,
    body: &str,
    want_id: u64,
) -> Result<Value, String> {
    if content_type.contains("text/event-stream") {
        parse_sse_body(body, want_id)
    } else {
        serde_json::from_str::<Value>(body).map_err(|e| format!("mcp response not JSON: {e}"))
    }
}

/// Scan an SSE body for JSON-RPC events: each event is one or more consecutive `data:` lines
/// (joined with `\n` per the SSE spec) terminated by a blank line. Priming/ping events whose data
/// isn't JSON are skipped rather than failing the whole parse. Among the JSON events found,
/// prefer the one whose `"id"` matches `want_id` (a server may multiplex unrelated notifications
/// on the same stream); otherwise fall back to the last event, since Streamable-HTTP servers emit
/// exactly one JSON-RPC message per response in practice.
fn parse_sse_body(body: &str, want_id: u64) -> Result<Value, String> {
    let mut events: Vec<Value> = Vec::new();
    let mut data_lines: Vec<&str> = Vec::new();

    let flush = |data_lines: &mut Vec<&str>, events: &mut Vec<Value>| {
        if data_lines.is_empty() {
            return;
        }
        if let Ok(v) = serde_json::from_str::<Value>(&data_lines.join("\n")) {
            events.push(v);
        }
        data_lines.clear();
    };

    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.strip_prefix(' ').unwrap_or(rest));
        } else if line.trim().is_empty() {
            flush(&mut data_lines, &mut events);
        }
        // Other SSE fields (`event:`, `id:`, `:comment`) carry no JSON-RPC content — ignored.
    }
    flush(&mut data_lines, &mut events); // a final event with no trailing blank line

    if events.is_empty() {
        return Err("mcp response: SSE stream contained no parsable JSON-RPC event".to_string());
    }
    let matched = events
        .iter()
        .find(|e| e.get("id").and_then(Value::as_u64) == Some(want_id));
    Ok(matched
        .cloned()
        .unwrap_or_else(|| events.last().unwrap().clone()))
}

/// Unwrap a JSON-RPC envelope (`{"jsonrpc","id","result"}` or `{"jsonrpc","id","error"}`) into
/// its `result`, or a readable failure string built from the `error` object.
pub(crate) fn extract_result_or_error(envelope: Value) -> Result<Value, String> {
    if let Some(err) = envelope.get("error") {
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(match err.get("code").and_then(Value::as_i64) {
            Some(code) => format!("mcp error {code}: {message}"),
            None => format!("mcp error: {message}"),
        });
    }
    Ok(envelope.get("result").cloned().unwrap_or(Value::Null))
}

/// Parse a `tools/list` `result` object into [`McpTool`]s. A tool missing `name`/`description`
/// degrades to an empty string rather than dropping the tool — a malformed entry from a
/// third-party server should not hide every *other* tool it advertised.
pub(crate) fn parse_tools(result: &Value) -> Result<Vec<McpTool>, String> {
    let tools = result
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| "mcp tools/list result missing \"tools\" array".to_string())?;
    Ok(tools
        .iter()
        .map(|t| McpTool {
            name: t
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            description: t
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            input_schema: t.get("inputSchema").cloned().unwrap_or(Value::Null),
        })
        .collect())
}

/// Parse a `tools/call` `result` object: flatten every `content[]` entry of `"type": "text"` into
/// one newline-joined string. `isError: true` is content, not a transport error (matches
/// `ai::web::execute_tool`'s degrade rule) — it comes back `Ok`, prefixed so the model reading it
/// can tell the call failed.
pub(crate) fn parse_call_result(result: &Value) -> Result<String, String> {
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|c| c.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|c| c.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();

    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if is_error {
        Ok(format!("tool error: {text}"))
    } else {
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parse_rpc_response ----------------------------------------------------------------

    #[test]
    fn plain_json_response_parses() {
        let body = r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}"#;
        let v = parse_rpc_response("application/json", body, 2).unwrap();
        assert_eq!(v["result"]["tools"], json!([]));
    }

    #[test]
    fn plain_json_with_charset_suffix_still_recognized_as_json() {
        // content-type params (e.g. charset) must not defeat the SSE/plain-JSON branch choice.
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{}}"#;
        let v = parse_rpc_response("application/json; charset=utf-8", body, 1).unwrap();
        assert_eq!(v["id"], json!(1));
    }

    #[test]
    fn sse_single_event_single_line_data() {
        let body = "event: message\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"ok\":true}}\r\n\r\n";
        let v = parse_rpc_response("text/event-stream", body, 3).unwrap();
        assert_eq!(v["result"]["ok"], json!(true));
    }

    #[test]
    fn sse_two_events_picks_matching_id() {
        // A notification (id absent) followed by the real response — matching id must win even
        // though it is not the last event... here it IS the last, so also test the reverse order.
        let body = concat!(
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n",
            "\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":5,\"result\":{\"tools\":[{\"name\":\"x\"}]}}\n",
            "\n",
        );
        let v = parse_rpc_response("text/event-stream", body, 5).unwrap();
        assert_eq!(v["result"]["tools"][0]["name"], json!("x"));

        // Reverse order: the id-5 response arrives first, a trailing notification after it.
        let body_rev = concat!(
            "data: {\"jsonrpc\":\"2.0\",\"id\":5,\"result\":{\"tools\":[{\"name\":\"x\"}]}}\n",
            "\n",
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n",
            "\n",
        );
        let v = parse_rpc_response("text/event-stream", body_rev, 5).unwrap();
        assert_eq!(v["result"]["tools"][0]["name"], json!("x"));
    }

    #[test]
    fn sse_multiline_data_is_joined() {
        // Per the SSE spec, consecutive `data:` lines within one event join with '\n'.
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\ndata: \"result\":{\"ok\":1}}\n\n";
        let v = parse_rpc_response("text/event-stream", body, 1).unwrap();
        assert_eq!(v["result"]["ok"], json!(1));
    }

    #[test]
    fn sse_event_without_trailing_blank_line_still_parses() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":9,\"result\":{}}";
        let v = parse_rpc_response("text/event-stream", body, 9).unwrap();
        assert_eq!(v["id"], json!(9));
    }

    #[test]
    fn sse_ping_priming_event_is_skipped_not_fatal() {
        // A non-JSON keepalive/comment-only "event" must not abort the whole parse.
        let body = concat!(
            "data: ping\n",
            "\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n",
            "\n",
        );
        let v = parse_rpc_response("text/event-stream", body, 1).unwrap();
        assert_eq!(v["result"]["ok"], json!(true));
    }

    #[test]
    fn sse_no_parsable_events_is_readable_error() {
        let err = parse_rpc_response("text/event-stream", "data: ping\n\n", 1).unwrap_err();
        assert!(err.contains("no parsable"));
    }

    #[test]
    fn plain_json_malformed_is_readable_error() {
        let err = parse_rpc_response("application/json", "not json", 1).unwrap_err();
        assert!(err.contains("not JSON"));
    }

    // ---- extract_result_or_error ------------------------------------------------------------

    #[test]
    fn extract_result_success() {
        let envelope = json!({"jsonrpc":"2.0","id":1,"result":{"tools":[]}});
        assert_eq!(
            extract_result_or_error(envelope).unwrap(),
            json!({"tools":[]})
        );
    }

    #[test]
    fn extract_result_missing_defaults_to_null() {
        let envelope = json!({"jsonrpc":"2.0","id":1});
        assert_eq!(extract_result_or_error(envelope).unwrap(), Value::Null);
    }

    #[test]
    fn extract_error_object_becomes_readable_string() {
        let envelope =
            json!({"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"Method not found"}});
        let err = extract_result_or_error(envelope).unwrap_err();
        assert!(err.contains("-32601"));
        assert!(err.contains("Method not found"));
    }

    #[test]
    fn extract_error_without_code_still_readable() {
        let envelope = json!({"jsonrpc":"2.0","id":1,"error":{"message":"boom"}});
        let err = extract_result_or_error(envelope).unwrap_err();
        assert_eq!(err, "mcp error: boom");
    }

    // ---- parse_tools --------------------------------------------------------------------------

    #[test]
    fn parse_tools_extracts_full_shape() {
        let result = json!({
            "tools": [
                {"name": "search", "description": "search things", "inputSchema": {"type": "object"}},
                {"name": "no_desc"},
            ]
        });
        let tools = parse_tools(&result).unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "search");
        assert_eq!(tools[0].description, "search things");
        assert_eq!(tools[0].input_schema, json!({"type": "object"}));
        assert_eq!(tools[1].name, "no_desc");
        assert_eq!(tools[1].description, "");
        assert_eq!(tools[1].input_schema, Value::Null);
    }

    #[test]
    fn parse_tools_missing_array_is_error() {
        let err = parse_tools(&json!({})).unwrap_err();
        assert!(err.contains("tools"));
    }

    // ---- parse_call_result ---------------------------------------------------------------------

    #[test]
    fn call_result_flattens_text_blocks() {
        let result = json!({
            "content": [
                {"type": "text", "text": "first"},
                {"type": "text", "text": "second"},
                {"type": "image", "data": "ignored"},
            ]
        });
        assert_eq!(parse_call_result(&result).unwrap(), "first\nsecond");
    }

    #[test]
    fn call_result_is_error_prefixes_text() {
        let result = json!({
            "content": [{"type": "text", "text": "tool blew up"}],
            "isError": true,
        });
        assert_eq!(
            parse_call_result(&result).unwrap(),
            "tool error: tool blew up"
        );
    }

    #[test]
    fn call_result_no_content_is_empty_string_not_error() {
        assert_eq!(parse_call_result(&json!({})).unwrap(), "");
    }

    // ---- McpClient::new / construction ---------------------------------------------------------

    #[test]
    fn client_construction_succeeds_with_and_without_token() {
        assert!(McpClient::new("http://localhost:9999/mcp", None).is_ok());
        assert!(McpClient::new("http://localhost:9999/mcp", Some("tok".to_string())).is_ok());
    }
}
