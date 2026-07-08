//! A second LLM backend that shells out to the local `claude` CLI (docs/adr/0009).
//!
//! Unlike the Ollama backend (a pure text model over HTTP), claude-code is *agentic*: pointed at
//! the owner's directories it can Read/Grep/Glob/Bash their Obsidian vault and Claude Code
//! artifacts while interrogating an idea. The wire pattern is lifted from
//! `ai-automation/claude-remote-chat`: spawn `claude --output-format stream-json`, write one
//! user-message JSON line on stdin, and parse the newline-delimited JSON on stdout, forwarding
//! `text_delta` chunks as tokens.
//!
//! idea-vault reassembles the full budgeted context every turn (stateless prompt-per-turn), so no
//! `--resume`/session state is needed here: each call is a fresh one-shot `claude` process. The
//! returned stream owns the child; dropping it (client disconnect, done) kills the process
//! (`kill_on_drop`), so the persist-nothing-on-abort boundary (D11) holds unchanged.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use futures::StreamExt;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdout, Command};

use crate::ai::ollama::{ChatMessage, TokenStream};
use crate::ai::{AiError, AiHealth};

/// Client that runs the `claude` CLI as the LLM backend. Cheap to clone (holds only config).
#[derive(Clone)]
pub struct ClaudeCodeClient {
    binary: String,
    cwd: PathBuf,
    add_dirs: Vec<PathBuf>,
    allowed_tools: Vec<String>,
    disallowed_tools: Vec<String>,
    model: Option<String>,
    system_prompt: Option<String>,
    skip_permissions: bool,
    token_timeout: Duration,
    mcp_config_json: Option<String>,
}

/// How the client is configured from `config.rs` (keeps the constructor from growing arguments).
#[derive(Debug, Clone)]
pub struct ClaudeCodeConfig {
    pub binary: String,
    pub cwd: PathBuf,
    pub add_dirs: Vec<PathBuf>,
    pub allowed_tools: Vec<String>,
    /// Tools the CLI must NOT use (`--disallowedTools`). A deny wins over everything, including
    /// `--dangerously-skip-permissions` — how the web-access toggle keeps an off state honest
    /// (ADR-0017).
    pub disallowed_tools: Vec<String>,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub skip_permissions: bool,
    pub token_timeout: Duration,
    /// Rendered `--mcp-config` JSON (`ai::backend::claude_mcp_config_json` builds it from the
    /// enabled-server registry per call). `Some` ⇒ spawn with `--mcp-config <tmpfile>` +
    /// `--strict-mcp-config` (only OUR servers, not whatever `.mcp.json` the cwd happens to
    /// contain); `None` ⇒ no MCP flags at all.
    pub mcp_config_json: Option<String>,
}

impl ClaudeCodeClient {
    pub fn new(cfg: ClaudeCodeConfig) -> Self {
        Self {
            binary: cfg.binary,
            cwd: cfg.cwd,
            add_dirs: cfg.add_dirs,
            allowed_tools: cfg.allowed_tools,
            disallowed_tools: cfg.disallowed_tools,
            model: cfg.model,
            system_prompt: cfg.system_prompt,
            skip_permissions: cfg.skip_permissions,
            token_timeout: cfg.token_timeout,
            mcp_config_json: cfg.mcp_config_json,
        }
    }

    /// A human-facing model label (there is no model list to probe as with Ollama).
    pub fn model(&self) -> &str {
        self.model.as_deref().unwrap_or("claude-code")
    }

    /// Health probe: does the `claude` binary run at all? `claude --version` succeeding is treated
    /// as [`AiHealth::Available`]; anything else is [`AiHealth::Unreachable`]. (There is no
    /// `ModelMissing` analogue — an auth failure surfaces per-call as an [`AiError::Backend`].)
    ///
    /// Every non-`Available` outcome is `tracing::warn!`-logged with its distinct cause
    /// (spawn error / non-zero exit + stderr / timeout) — otherwise "unreachable" is
    /// undiagnosable, and the most common cause (the binary not being on the server's PATH)
    /// looks identical to an auth or version failure.
    pub async fn probe(&self) -> AiHealth {
        // `.output()` (not `.status()`) so a non-zero exit's stderr is captured for the log.
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            Command::new(&self.binary)
                .arg("--version")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .output(),
        )
        .await;
        match result {
            // Ran and exited 0 — the CLI is usable.
            Ok(Ok(output)) if output.status.success() => AiHealth::Available,
            // Ran but exited non-zero — surface the code + stderr so the cause is visible.
            Ok(Ok(output)) => {
                tracing::warn!(
                    binary = %self.binary,
                    code = output.status.code().unwrap_or(-1),
                    stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                    "claude-code probe: `claude --version` exited non-zero"
                );
                AiHealth::Unreachable
            }
            // Failed to spawn — almost always the binary isn't on the server process's PATH.
            Ok(Err(e)) => {
                tracing::warn!(
                    binary = %self.binary,
                    error = %e,
                    "claude-code probe: could not run `claude` — is it installed and on the \
                     server's PATH? Set IDEA_VAULT_CLAUDE_BIN to its absolute path"
                );
                AiHealth::Unreachable
            }
            // Exceeded the 5s bound.
            Err(_) => {
                tracing::warn!(
                    binary = %self.binary,
                    "claude-code probe: `claude --version` did not return within 5s"
                );
                AiHealth::Unreachable
            }
        }
    }

    /// Non-streaming completion: consume [`chat_stream`](Self::chat_stream) to the end and return
    /// the concatenated text. Any stream error aborts the whole call (nothing partial returned).
    pub async fn chat(&self, messages: Vec<ChatMessage>) -> Result<String, AiError> {
        let mut stream = self.chat_stream(messages).await?;
        let mut out = String::new();
        while let Some(item) = stream.next().await {
            out.push_str(&item?);
        }
        Ok(out)
    }

    /// Flatten the (usually single) budgeted user message into one prompt string for the CLI.
    fn flatten_prompt(messages: &[ChatMessage]) -> String {
        if messages.len() == 1 {
            return messages[0].content.clone();
        }
        messages
            .iter()
            .map(|m| format!("[{}]\n{}", m.role, m.content))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// Stream a chat completion from the `claude` CLI (docs/adr/0009). Yields text chunks in order;
    /// the stream ends on the `result` event. Tool activity (Grep/Read/Bash the foil performs) is
    /// consumed but not streamed as chat tokens — only the model's prose reaches the transcript.
    pub async fn chat_stream(&self, messages: Vec<ChatMessage>) -> Result<TokenStream, AiError> {
        let prompt = Self::flatten_prompt(&messages);
        let user_message = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": prompt },
        })
        .to_string();

        let mut cmd = Command::new(&self.binary);
        cmd.arg("--output-format")
            .arg("stream-json")
            .arg("--input-format")
            .arg("stream-json")
            .arg("--verbose")
            .arg("--include-partial-messages");

        if self.skip_permissions {
            cmd.arg("--dangerously-skip-permissions");
        } else if !self.allowed_tools.is_empty() {
            cmd.arg("--allowedTools").arg(self.allowed_tools.join(","));
        }
        if !self.disallowed_tools.is_empty() {
            // Passed in every mode: a deny must hold even under --dangerously-skip-permissions.
            cmd.arg("--disallowedTools")
                .arg(self.disallowed_tools.join(","));
        }
        for dir in &self.add_dirs {
            cmd.arg("--add-dir").arg(dir);
        }
        if let Some(model) = &self.model {
            cmd.arg("--model").arg(model);
        }
        if let Some(sys) = &self.system_prompt {
            cmd.arg("--append-system-prompt").arg(sys);
        }
        let mcp_config_path = if let Some(json) = &self.mcp_config_json {
            // The CLI reads the config file once at spawn, so a per-call temp file under the OS
            // temp dir is enough — a unique (pid + counter) name keeps concurrent turns apart.
            // The rendered JSON embeds MCP bearer tokens, so this is a SECRET at rest: written
            // owner-only (0600 on unix; the default temp dir is world-shared) and deleted when
            // the stream state drops (turn done / cancelled / client killed) — never left for
            // other local users to harvest.
            static MCP_TMP_COUNTER: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            let n = MCP_TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("idea-vault-mcp-{}-{n}.json", std::process::id()));
            write_secret(&path, json).map_err(|e| {
                AiError::Backend(format!("writing mcp config {}: {e}", path.display()))
            })?;
            // --strict-mcp-config: use exactly these servers; ignore any user/project .mcp.json
            // in the foil's cwd (the vault), which the owner never intended as CLI config.
            cmd.arg("--mcp-config")
                .arg(&path)
                .arg("--strict-mcp-config");
            Some(path)
        } else {
            None
        };

        cmd.current_dir(&self.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            // Own process group so a terminal signal to the server doesn't hit the child.
            .process_group(0);

        let mut child = cmd
            .spawn()
            .map_err(|e| AiError::Backend(format!("failed to spawn `{}`: {e}", self.binary)))?;

        // Write the single user message, then close stdin — this is a one-shot turn, so the CLI
        // has all its input and will run to a `result` without us relaying anything further.
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| AiError::Backend("claude stdin unavailable".into()))?;
        let line = format!("{user_message}\n");
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| AiError::Backend(format!("writing prompt to claude: {e}")))?;
        stdin
            .flush()
            .await
            .map_err(|e| AiError::Backend(format!("flushing prompt to claude: {e}")))?;
        drop(stdin);

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AiError::Backend("claude stdout unavailable".into()))?;
        let lines = BufReader::new(stdout).lines();

        let state = StreamState {
            child,
            lines,
            token_timeout: self.token_timeout,
            emitted_any: false,
            finished: false,
            mcp_config_path,
        };

        Ok(futures::stream::unfold(state, next_token).boxed())
    }
}

struct StreamState {
    /// Held only to keep the process alive; dropping the state kills it (`kill_on_drop`), which is
    /// how a client disconnect / done aborts the `claude` run (D11 persist-nothing boundary).
    #[allow(dead_code)]
    child: Child,
    lines: Lines<BufReader<ChildStdout>>,
    token_timeout: Duration,
    emitted_any: bool,
    finished: bool,
    /// The per-call `--mcp-config` temp file (bearer tokens inside) — removed on drop, which
    /// fires whether the turn finished, errored, or was cancelled mid-stream.
    mcp_config_path: Option<PathBuf>,
}

impl Drop for StreamState {
    fn drop(&mut self) {
        if let Some(path) = self.mcp_config_path.take() {
            // Best-effort: the child has been killed/reaped by now (kill_on_drop) and the CLI
            // read the file at spawn; a failed unlink only means the 0600 file lingers.
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Write `contents` readable by the owner only (0600) — for files carrying credentials. On
/// non-unix targets this degrades to a plain write (no world-shared /tmp semantics there).
fn write_secret(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    // An existing file keeps its old mode; enforce 0600 even on overwrite.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    f.write_all(contents.as_bytes())
}

/// Pull the next text token (or terminal error) from the CLI's `stream-json` stdout. Skips tool
/// and bookkeeping events; ends on `result`; every read is bounded by the hard timeout (D20).
async fn next_token(mut st: StreamState) -> Option<(Result<String, AiError>, StreamState)> {
    if st.finished {
        return None;
    }
    loop {
        let line = match tokio::time::timeout(st.token_timeout, st.lines.next_line()).await {
            Err(_) => {
                st.finished = true;
                return Some((Err(AiError::Timeout), st));
            }
            Ok(Err(e)) => {
                st.finished = true;
                return Some((
                    Err(AiError::Backend(format!("reading claude output: {e}"))),
                    st,
                ));
            }
            Ok(Ok(None)) => {
                // stdout closed before a `result` — the process died mid-turn.
                st.finished = true;
                return Some((
                    Err(AiError::Backend("claude ended before a result".into())),
                    st,
                ));
            }
            Ok(Ok(Some(line))) => line,
        };

        match classify_line(&line) {
            Line::Token(text) => {
                if text.is_empty() {
                    continue;
                }
                st.emitted_any = true;
                return Some((Ok(text), st));
            }
            Line::AuthError(detail) => {
                st.finished = true;
                return Some((
                    Err(AiError::Backend(format!("claude auth failed: {detail}"))),
                    st,
                ));
            }
            Line::ErrorResult(detail) => {
                st.finished = true;
                return Some((Err(AiError::Backend(format!("claude error: {detail}"))), st));
            }
            Line::Result(result_text) => {
                st.finished = true;
                // If partial-message streaming produced nothing, fall back to the result text so
                // the turn is never silently empty.
                if !st.emitted_any {
                    if let Some(text) = result_text {
                        if !text.is_empty() {
                            return Some((Ok(text), st));
                        }
                    }
                }
                return None;
            }
            Line::Ignore => continue,
        }
    }
}

/// The only `stream-json` line shapes idea-vault cares about (text, auth failure, terminal result).
enum Line {
    Token(String),
    AuthError(String),
    Result(Option<String>),
    /// A terminal `result` with `is_error: true` — carries the error text (e.g. "Invalid API key ·
    /// Please run /login" for a bad/expired token) so the turn fails visibly instead of ending as
    /// a misleading empty reply.
    ErrorResult(String),
    Ignore,
}

/// Classify one `stream-json` stdout line. Lifted (and reduced to the text/result/auth subset) from
/// `claude-remote-chat/src/claude/parser.rs`.
fn classify_line(line: &str) -> Line {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return Line::Ignore;
    };
    match v.get("type").and_then(|t| t.as_str()) {
        // Streaming text lives inside stream_event → content_block_delta → text_delta.
        Some("stream_event") => {
            let inner = v.get("event");
            let inner_type = inner.and_then(|e| e.get("type")).and_then(|t| t.as_str());
            if inner_type == Some("content_block_delta") {
                if let Some(text) = text_delta(inner.unwrap()) {
                    return Line::Token(text);
                }
            }
            Line::Ignore
        }
        // Legacy top-level delta (non-stream_event mode).
        Some("content_block_delta") => text_delta(&v).map(Line::Token).unwrap_or(Line::Ignore),
        // An auth/API failure is reported on the assistant event's `error` field.
        Some("assistant") => match v.get("error").and_then(|e| e.as_str()) {
            Some(err @ ("authentication_failed" | "unauthorized")) => {
                let detail = v
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array())
                    .and_then(|a| a.first())
                    .and_then(|b| b.get("text"))
                    .and_then(|t| t.as_str())
                    .unwrap_or(err);
                Line::AuthError(detail.to_string())
            }
            _ => Line::Ignore,
        },
        Some("result") => {
            let text = v.get("result").and_then(|r| r.as_str()).map(str::to_string);
            if v.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false) {
                Line::ErrorResult(text.unwrap_or_else(|| "unknown error".into()))
            } else {
                Line::Result(text)
            }
        }
        _ => Line::Ignore,
    }
}

/// Extract `delta.text` from a `content_block_delta` value, if it is a `text_delta`.
fn text_delta(v: &serde_json::Value) -> Option<String> {
    let delta = v.get("delta")?;
    if delta.get("type").and_then(|t| t.as_str()) != Some("text_delta") {
        return None;
    }
    delta
        .get("text")
        .and_then(|t| t.as_str())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_streams_text_delta() {
        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hello"}}}"#;
        assert!(matches!(classify_line(line), Line::Token(t) if t == "Hello"));
    }

    #[test]
    fn classify_result_carries_fallback_text() {
        let line = r#"{"type":"result","result":"final text"}"#;
        assert!(matches!(classify_line(line), Line::Result(Some(t)) if t == "final text"));
    }

    #[test]
    fn classify_error_result_surfaces_text() {
        let line = r#"{"type":"result","is_error":true,"result":"boom"}"#;
        assert!(matches!(classify_line(line), Line::ErrorResult(t) if t == "boom"));
    }

    #[test]
    fn classify_error_result_without_text_still_errors() {
        let line = r#"{"type":"result","is_error":true}"#;
        assert!(matches!(classify_line(line), Line::ErrorResult(t) if t == "unknown error"));
    }

    #[test]
    fn classify_auth_failure() {
        let line = r#"{"type":"assistant","error":"authentication_failed","message":{"content":[{"type":"text","text":"401"}]}}"#;
        assert!(matches!(classify_line(line), Line::AuthError(d) if d.contains("401")));
    }

    #[test]
    fn classify_ignores_tool_and_noise() {
        assert!(matches!(
            classify_line(
                r#"{"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"tool_use","name":"Grep"}}}"#
            ),
            Line::Ignore
        ));
        assert!(matches!(
            classify_line(r#"{"type":"system","subtype":"init"}"#),
            Line::Ignore
        ));
        assert!(matches!(classify_line("not json"), Line::Ignore));
    }

    #[test]
    fn flatten_single_message_is_verbatim() {
        let msgs = vec![ChatMessage {
            role: "user".into(),
            content: "just this".into(),
        }];
        assert_eq!(ClaudeCodeClient::flatten_prompt(&msgs), "just this");
    }
}
