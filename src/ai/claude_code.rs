//! A second LLM backend that shells out to the local `claude` CLI (docs/adr/0009).
//!
//! Unlike the Ollama backend (a pure text model over HTTP), claude-code is *agentic*: it can
//! Read/Grep/Glob the idea it is interrogating and the owner's attached reference sources. The wire
//! pattern is lifted from `ai-automation/claude-remote-chat`: spawn `claude --output-format
//! stream-json`, write one user-message JSON line on stdin, and parse the newline-delimited JSON on
//! stdout, forwarding `text_delta` chunks as tokens.
//!
//! The foil runs locked down (ADR-0039): `--restricted` confines file tools to its cwd (the idea's
//! folder) and the `--add-dir` roots, `--tools` makes only [`FOIL_TOOLS`] (plus
//! [`FOIL_WEB_TOOLS`] while web access is on) exist at all, `--strict-mcp-config` always pins MCP
//! to exactly the servers idea-vault registered (none ⇒ an empty config), and no code path passes
//! `--dangerously-skip-permissions`. The child gets a scrubbed environment ([`CLAUDE_ENV_PASS`]),
//! so server secrets such as `IDEA_VAULT_MCP_TOKEN` never reach it, and the stream-json `init`
//! event is checked against the allowlist ([`check_init`]) before any output is accepted.
//!
//! idea-vault reassembles the full budgeted context every turn (stateless prompt-per-turn), so no
//! `--resume`/session state is needed here: each call is a fresh one-shot `claude` process. The
//! returned stream owns the child; dropping it (client disconnect, done, turn deadline) kills the
//! process (`kill_on_drop`), so the persist-nothing-on-abort boundary (D11) holds unchanged.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use futures::StreamExt;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdout, Command};

use crate::ai::call::{fill_slot, meta_slot, read_slot, CallMeta, CallUsage, MetaSlot};
use crate::ai::ollama::{ChatMessage, TokenStream};
use crate::ai::{AiError, AiHealth};

/// The only built-in tools the foil has (ADR-0039): it reads, never writes or executes. Passed as
/// `--tools`, which removes every other built-in from the session rather than merely leaving it
/// unapproved.
pub const FOIL_TOOLS: &[&str] = &["Read", "Grep", "Glob"];

/// Added to [`FOIL_TOOLS`] only while the live web-access toggle is on (ADR-0017).
pub const FOIL_WEB_TOOLS: &[&str] = &["WebSearch", "WebFetch"];

/// The permission rule that keeps the foil out of the run journal under its cwd
/// (`vault/<slug>/.runs/`, ADR-0037): a `Read` deny, which the CLI also applies to Grep and Glob.
pub const RUN_JOURNAL_DENY: &str = "Read(./.runs/**)";

/// The foil's MCP config when no server is registered: `--strict-mcp-config` still needs a file,
/// and an empty one strips any user-level servers the CLI would otherwise inherit (ADR-0039).
pub const EMPTY_MCP_CONFIG: &str = r#"{"mcpServers":{}}"#;

/// A whole foil turn's default wall-clock ceiling (ADR-0039; `IDEA_VAULT_CLAUDE_TURN_TIMEOUT_SECS`
/// overrides it): generous, because a timed-out turn persists nothing (D11), yet finite, because a
/// foil busy with tool calls never trips the per-line timeout.
pub const DEFAULT_TURN_TIMEOUT: Duration = Duration::from_secs(1800);

/// Environment keys the claude child may inherit (ADR-0039): what the CLI needs to find itself,
/// authenticate, and reach the network through a proxy. Everything else is withheld, and every
/// `IDEA_VAULT_*` key always is. Extended by `IDEA_VAULT_CLAUDE_ENV_PASS`.
pub const CLAUDE_ENV_PASS: &[&str] = &[
    "HOME",
    "PATH",
    "USER",
    "SHELL",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "TMPDIR",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "XDG_DATA_HOME",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CONFIG_DIR",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_BASE_URL",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "NODE_EXTRA_CA_CERTS",
];

/// The prefix of every idea-vault setting. Never passed to the child, even when the owner lists a
/// key in `IDEA_VAULT_CLAUDE_ENV_PASS`, because the server's own secrets live under it.
const APP_ENV_PREFIX: &str = "IDEA_VAULT_";

/// The environment the claude child is spawned with: [`CLAUDE_ENV_PASS`] plus `extra_pass`, minus
/// every `IDEA_VAULT_*` key.
fn child_env(extra_pass: &[String]) -> Vec<(OsString, OsString)> {
    child_env_from(std::env::vars_os(), extra_pass)
}

/// [`child_env`] over an explicit variable list, so the filter is testable without touching the
/// process environment.
fn child_env_from(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
    extra_pass: &[String],
) -> Vec<(OsString, OsString)> {
    vars.into_iter()
        .filter(|(key, _)| {
            let Some(key) = key.to_str() else {
                return false;
            };
            !key.starts_with(APP_ENV_PREFIX)
                && (CLAUDE_ENV_PASS.contains(&key) || extra_pass.iter().any(|p| p.trim() == key))
        })
        .collect()
}

/// The built-in tools the foil runs with for this web-access state.
pub fn foil_tools(web_access: bool) -> Vec<&'static str> {
    let mut tools = FOIL_TOOLS.to_vec();
    if web_access {
        tools.extend_from_slice(FOIL_WEB_TOOLS);
    }
    tools
}

/// Check a stream-json `init` event against the lockdown (ADR-0039): every built-in tool it lists
/// must be in `allowed`, and every `mcp__<server>__…` tool and every listed MCP server must be one
/// idea-vault registered. A missing `tools` list is a violation too, because an unverifiable
/// session is not accepted. Allowlisted tools the session lacks are only logged.
pub fn check_init(
    init: &serde_json::Value,
    allowed: &[&str],
    mcp_servers: &BTreeSet<String>,
) -> Result<(), String> {
    let Some(tools) = init.get("tools").and_then(|t| t.as_array()) else {
        return Err("the init event carried no tools list".into());
    };
    let mut unexpected: Vec<String> = Vec::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for tool in tools {
        let name = tool.as_str().unwrap_or_default();
        seen.insert(name);
        let ok = match name.strip_prefix("mcp__") {
            Some(rest) => rest
                .split_once("__")
                .is_some_and(|(server, _)| mcp_servers.contains(server)),
            None => allowed.contains(&name),
        };
        if !ok {
            unexpected.push(name.to_string());
        }
    }
    if let Some(servers) = init.get("mcp_servers").and_then(|m| m.as_array()) {
        for server in servers {
            let name = server
                .get("name")
                .and_then(|n| n.as_str())
                .or_else(|| server.as_str())
                .unwrap_or_default();
            if !mcp_servers.contains(name) {
                unexpected.push(format!("mcp server {name}"));
            }
        }
    }
    if !unexpected.is_empty() {
        return Err(format!(
            "the foil session exposes {} outside its allowlist",
            unexpected.join(", ")
        ));
    }
    let missing: Vec<&str> = allowed
        .iter()
        .copied()
        .filter(|t| !seen.contains(t))
        .collect();
    if !missing.is_empty() {
        tracing::warn!(
            ?missing,
            "claude init lists fewer tools than the foil allowlist"
        );
    }
    Ok(())
}

/// Client that runs the `claude` CLI as the LLM backend. Cheap to clone (holds only config).
#[derive(Clone)]
pub struct ClaudeCodeClient {
    cfg: ClaudeCodeConfig,
}

/// How the client is configured from `config.rs` (keeps the constructor from growing arguments).
/// There is deliberately no permission-bypass field: the foil's reach is fixed by ADR-0039.
#[derive(Debug, Clone)]
pub struct ClaudeCodeConfig {
    pub binary: String,
    /// The foil's working directory: the idea's own folder on an idea turn (ADR-0039), which
    /// `--restricted` makes the edge of what its file tools may touch.
    pub cwd: PathBuf,
    /// Extra readable roots (`--add-dir`): owner reference dirs and attached sources (ADR-0021).
    pub add_dirs: Vec<PathBuf>,
    /// Pre-approvals (`--allowedTools`). They never widen the tool set: an entry outside
    /// [`foil_tools`] and the registered `mcp__<server>` prefixes is dropped.
    pub allowed_tools: Vec<String>,
    /// The live web-access toggle (ADR-0017): adds [`FOIL_WEB_TOOLS`] when on, and denies them
    /// explicitly when off.
    pub web_access: bool,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    /// Longest silence allowed between two output lines (D20).
    pub token_timeout: Duration,
    /// Wall-clock ceiling on a whole turn (ADR-0039). A foil busy with tool calls never trips the
    /// per-line timeout, so this is what bounds it.
    pub turn_timeout: Duration,
    /// Owner additions to [`CLAUDE_ENV_PASS`] (`IDEA_VAULT_CLAUDE_ENV_PASS`).
    pub env_pass: Vec<String>,
    /// Rendered `--mcp-config` JSON (`ai::backend::claude_mcp_config_json` builds it from the
    /// enabled-server registry per call). `None` ⇒ [`EMPTY_MCP_CONFIG`]; either way the CLI runs
    /// with `--strict-mcp-config`, so no user-level or cwd `.mcp.json` server reaches the foil.
    pub mcp_config_json: Option<String>,
}

impl ClaudeCodeConfig {
    /// Names of the MCP servers this config registers (the keys of `mcpServers`).
    pub fn mcp_server_names(&self) -> BTreeSet<String> {
        self.mcp_config_json
            .as_deref()
            .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
            .and_then(|v| v.get("mcpServers").and_then(|m| m.as_object()).cloned())
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// The pre-approval list: the foil tools, then whichever owner or router entries name one of
    /// them or a registered MCP server. Anything else could not run anyway and is not passed.
    fn approvals(&self) -> Vec<String> {
        let tools = foil_tools(self.web_access);
        let mcp = self.mcp_server_names();
        let permitted = |entry: &str| {
            let base = entry.split('(').next().unwrap_or(entry).trim();
            tools.contains(&base)
                || mcp.iter().any(|name| {
                    let prefix = format!("mcp__{name}");
                    base == prefix || base.starts_with(&format!("{prefix}__"))
                })
        };
        let mut out: Vec<String> = tools.iter().map(|t| t.to_string()).collect();
        for entry in &self.allowed_tools {
            if permitted(entry) && !out.contains(entry) {
                out.push(entry.clone());
            }
        }
        out
    }

    /// The deny rules. Always [`RUN_JOURNAL_DENY`]: the idea's run journal sits inside the foil's
    /// cwd, and it holds every earlier call's verbatim answer, REFUTED findings and fetched text
    /// included, which the foil must never read back as if it were the discussion (ADR-0037,
    /// ADR-0039). With web access off, also the web tools: a deny on top of the absent tool, so
    /// the off state stays honest even if a CLI version ever widened `--tools` (ADR-0017).
    fn denials(&self) -> Vec<String> {
        let mut out = vec![RUN_JOURNAL_DENY.to_string()];
        if !self.web_access {
            out.extend(FOIL_WEB_TOOLS.iter().map(|t| t.to_string()));
        }
        out
    }

    /// The CLI argument vector for one turn, given where the MCP config file was written. Pure, so
    /// the lockdown (ADR-0039) is assertable without spawning anything.
    pub fn args(&self, mcp_config_path: &Path) -> Vec<OsString> {
        let mut args: Vec<OsString> = [
            "--output-format",
            "stream-json",
            "--input-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--restricted",
            "--tools",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        args.push(foil_tools(self.web_access).join(",").into());
        args.push("--allowedTools".into());
        args.push(self.approvals().join(",").into());
        args.push("--disallowedTools".into());
        args.push(self.denials().join(",").into());
        for dir in &self.add_dirs {
            args.push("--add-dir".into());
            args.push(dir.into());
        }
        if let Some(model) = &self.model {
            args.push("--model".into());
            args.push(model.into());
        }
        if let Some(sys) = &self.system_prompt {
            args.push("--append-system-prompt".into());
            args.push(sys.into());
        }
        args.push("--mcp-config".into());
        args.push(mcp_config_path.into());
        args.push("--strict-mcp-config".into());
        args
    }
}

impl ClaudeCodeClient {
    pub fn new(cfg: ClaudeCodeConfig) -> Self {
        Self { cfg }
    }

    /// A human-facing model label (there is no model list to probe as with Ollama).
    pub fn model(&self) -> &str {
        self.cfg.model.as_deref().unwrap_or("claude-code")
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
        // `.output()` (not `.status()`) so a non-zero exit's stderr is captured for the log. The
        // probe gets the same scrubbed environment as a turn (ADR-0039).
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            Command::new(&self.cfg.binary)
                .arg("--version")
                .env_clear()
                .envs(child_env(&self.cfg.env_pass))
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
                    binary = %self.cfg.binary,
                    code = output.status.code().unwrap_or(-1),
                    stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                    "claude-code probe: `claude --version` exited non-zero"
                );
                AiHealth::Unreachable
            }
            // Failed to spawn — almost always the binary isn't on the server process's PATH.
            Ok(Err(e)) => {
                tracing::warn!(
                    binary = %self.cfg.binary,
                    error = %e,
                    "claude-code probe: could not run `claude` — is it installed and on the \
                     server's PATH? Set IDEA_VAULT_CLAUDE_BIN to its absolute path"
                );
                AiHealth::Unreachable
            }
            // Exceeded the 5s bound.
            Err(_) => {
                tracing::warn!(
                    binary = %self.cfg.binary,
                    "claude-code probe: `claude --version` did not return within 5s"
                );
                AiHealth::Unreachable
            }
        }
    }

    /// Non-streaming completion: consume [`chat_stream`](Self::chat_stream) to the end and return
    /// the concatenated text. Any stream error aborts the whole call (nothing partial returned).
    pub async fn chat(&self, messages: Vec<ChatMessage>) -> Result<String, AiError> {
        self.chat_meta(messages).await.map(|(text, _)| text)
    }

    /// [`chat`](Self::chat) plus the call's [`CallMeta`] from the `result` line (docs/adr/0037):
    /// one CLI process is one request.
    pub async fn chat_meta(
        &self,
        messages: Vec<ChatMessage>,
    ) -> Result<(String, CallMeta), AiError> {
        let mut stream = self.chat_stream(messages).await?;
        let mut out = String::new();
        while let Some(item) = stream.next().await {
            out.push_str(&item?);
        }
        let meta = read_slot(&stream.meta()).unwrap_or_else(|| CallMeta {
            usage: CallUsage {
                api_calls: 1,
                ..CallUsage::default()
            },
            ..CallMeta::default()
        });
        Ok((out, meta))
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
    /// the stream ends on the `result` event. Tool activity (the Grep/Read the foil performs) is
    /// consumed but not streamed as chat tokens — only the model's prose reaches the transcript.
    pub async fn chat_stream(&self, messages: Vec<ChatMessage>) -> Result<TokenStream, AiError> {
        let started = Instant::now();
        let prompt = Self::flatten_prompt(&messages);
        let user_message = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": prompt },
        })
        .to_string();

        // The CLI reads the config file once at spawn, so a per-call temp file under the OS temp
        // dir is enough — a unique (pid + counter) name keeps concurrent turns apart. The rendered
        // JSON may embed MCP bearer tokens, so it is a SECRET at rest: written owner-only (0600 on
        // unix; the default temp dir is world-shared) and deleted when the stream state drops
        // (turn done / cancelled / deadline / client killed). The empty config is written the same
        // way so `--strict-mcp-config` is unconditional (ADR-0039).
        static MCP_TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = MCP_TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mcp_config_path =
            std::env::temp_dir().join(format!("idea-vault-mcp-{}-{n}.json", std::process::id()));
        let json = self
            .cfg
            .mcp_config_json
            .as_deref()
            .unwrap_or(EMPTY_MCP_CONFIG);
        write_secret(&mcp_config_path, json).map_err(|e| {
            AiError::Backend(format!(
                "writing mcp config {}: {e}",
                mcp_config_path.display()
            ))
        })?;
        // From here on the guard owns the file, so every early return below removes it.
        let mcp_guard = TempFile(Some(mcp_config_path.clone()));

        let mut cmd = Command::new(&self.cfg.binary);
        cmd.args(self.cfg.args(&mcp_config_path))
            .env_clear()
            .envs(child_env(&self.cfg.env_pass))
            .current_dir(&self.cfg.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            // Own process group so a terminal signal to the server doesn't hit the child.
            .process_group(0);

        let mut child = cmd
            .spawn()
            .map_err(|e| AiError::Backend(format!("failed to spawn `{}`: {e}", self.cfg.binary)))?;

        // Write the single user message, then close stdin — this is a one-shot turn, so the CLI
        // has all its input and will run to a `result` without us relaying anything further.
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| AiError::Backend("claude stdin unavailable".into()))?;
        let line = format!("{user_message}\n");
        // Bounded like every stdout read (D20): a prompt larger than the pipe buffer blocks until
        // the CLI reads it, and a CLI that never does would otherwise hang the turn.
        let write = async {
            stdin
                .write_all(line.as_bytes())
                .await
                .map_err(|e| AiError::Backend(format!("writing prompt to claude: {e}")))?;
            stdin
                .flush()
                .await
                .map_err(|e| AiError::Backend(format!("flushing prompt to claude: {e}")))
        };
        tokio::time::timeout(self.cfg.token_timeout, write)
            .await
            .map_err(|_| AiError::Timeout)??;
        drop(stdin);

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AiError::Backend("claude stdout unavailable".into()))?;
        let lines = BufReader::new(stdout).lines();

        let meta = meta_slot();
        let state = StreamState {
            started,
            meta: meta.clone(),
            child,
            lines,
            token_timeout: self.cfg.token_timeout,
            deadline: started + self.cfg.turn_timeout,
            turn_timeout: self.cfg.turn_timeout,
            allowed_tools: foil_tools(self.cfg.web_access),
            mcp_servers: self.cfg.mcp_server_names(),
            init_checked: false,
            emitted_any: false,
            finished: false,
            _mcp_config: mcp_guard,
        };

        Ok(TokenStream::new(
            futures::stream::unfold(state, next_token).boxed(),
            meta,
        ))
    }
}

/// A temp file removed on drop: the per-call `--mcp-config` (bearer tokens inside), removed
/// whether the turn finished, errored, hit its deadline, or was cancelled mid-stream.
struct TempFile(Option<PathBuf>);

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            // Best-effort: the child has been killed/reaped by now (kill_on_drop) and the CLI
            // read the file at spawn; a failed unlink only means the 0600 file lingers.
            let _ = std::fs::remove_file(path);
        }
    }
}

struct StreamState {
    /// When the process was spawned: the call's wall clock for its [`CallMeta`].
    started: Instant,
    /// Filled from the `result` line (docs/adr/0037).
    meta: MetaSlot,
    /// Held only to keep the process alive; dropping the state kills it (`kill_on_drop`), which is
    /// how a client disconnect / done / deadline aborts the `claude` run (D11 persist-nothing).
    #[allow(dead_code)]
    child: Child,
    lines: Lines<BufReader<ChildStdout>>,
    token_timeout: Duration,
    /// When the whole turn must be over (ADR-0039), however busy the foil keeps its stdout.
    deadline: Instant,
    turn_timeout: Duration,
    /// What the `init` event may list ([`check_init`]).
    allowed_tools: Vec<&'static str>,
    mcp_servers: BTreeSet<String>,
    /// Set once the `init` event passed [`check_init`]; no output is accepted before that.
    init_checked: bool,
    emitted_any: bool,
    finished: bool,
    /// Declared last so it drops after `child`: the file outlives the process that reads it.
    _mcp_config: TempFile,
}

/// Write `contents` readable by the owner only (0600) — for files carrying credentials. On
/// non-unix targets this degrades to a plain write (no world-shared /tmp semantics there).
fn write_secret(path: &Path, contents: &str) -> std::io::Result<()> {
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

/// End the stream with `err` (terminal: the state is marked finished).
fn fail(mut st: StreamState, err: AiError) -> Option<(Result<String, AiError>, StreamState)> {
    st.finished = true;
    Some((Err(err), st))
}

/// Pull the next text token (or terminal error) from the CLI's `stream-json` stdout. Skips tool
/// and bookkeeping events; ends on `result`. Every read is bounded by the per-line timeout (D20)
/// and by what is left of the turn's wall-clock deadline (ADR-0039).
async fn next_token(mut st: StreamState) -> Option<(Result<String, AiError>, StreamState)> {
    if st.finished {
        return None;
    }
    loop {
        let left = st.deadline.saturating_duration_since(Instant::now());
        let wait = st.token_timeout.min(left);
        let line = match tokio::time::timeout(wait, st.lines.next_line()).await {
            Err(_) if Instant::now() >= st.deadline => {
                let secs = st.turn_timeout.as_secs_f64();
                return fail(
                    st,
                    AiError::Backend(format!("claude turn exceeded {secs}s wall clock")),
                );
            }
            Err(_) => return fail(st, AiError::Timeout),
            Ok(Err(e)) => return fail(st, AiError::Backend(format!("reading claude output: {e}"))),
            // stdout closed before a `result` — the process died mid-turn.
            Ok(Ok(None)) => {
                return fail(st, AiError::Backend("claude ended before a result".into()))
            }
            Ok(Ok(Some(line))) => line,
        };

        let line = classify_line(&line);
        if !st.init_checked && matches!(line, Line::Token(_) | Line::Result { .. }) {
            return fail(
                st,
                AiError::Backend(
                    "claude produced output before its init event, so the foil's tool \
                     allowlist could not be verified"
                        .into(),
                ),
            );
        }
        match line {
            Line::Init(init) => {
                if let Err(detail) = check_init(&init, &st.allowed_tools, &st.mcp_servers) {
                    return fail(
                        st,
                        AiError::Backend(format!("claude foil lockdown refused: {detail}")),
                    );
                }
                st.init_checked = true;
            }
            Line::Token(text) => {
                if text.is_empty() {
                    continue;
                }
                st.emitted_any = true;
                return Some((Ok(text), st));
            }
            Line::AuthError(detail) => {
                return fail(
                    st,
                    AiError::Backend(format!("claude auth failed: {detail}")),
                )
            }
            Line::ErrorResult(detail) => {
                return fail(st, AiError::Backend(format!("claude error: {detail}")))
            }
            Line::Result {
                text: result_text,
                usage,
                subtype,
            } => {
                st.finished = true;
                let ms = u64::try_from(st.started.elapsed().as_millis()).unwrap_or(u64::MAX);
                fill_slot(
                    &st.meta,
                    CallMeta {
                        usage,
                        stop_reason: subtype,
                        ms,
                        ..CallMeta::default()
                    },
                );
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

/// The only `stream-json` line shapes idea-vault cares about (init, text, auth failure, result).
enum Line {
    /// The `system`/`init` event that opens every session: its `tools` and `mcp_servers` are what
    /// [`check_init`] verifies.
    Init(serde_json::Value),
    Token(String),
    AuthError(String),
    /// The terminal success event: its fallback text, what the session cost (one CLI process is
    /// one request) and its subtype, which is the call's stop reason (docs/adr/0037).
    Result {
        text: Option<String>,
        usage: CallUsage,
        subtype: Option<String>,
    },
    /// A terminal `result` with `is_error: true` — carries the error text (e.g. "Invalid API key ·
    /// Please run /login" for a bad/expired token) so the turn fails visibly instead of ending as
    /// a misleading empty reply.
    ErrorResult(String),
    Ignore,
}

/// Classify one `stream-json` stdout line. Lifted (and reduced to the init/text/result/auth
/// subset) from `claude-remote-chat/src/claude/parser.rs`.
fn classify_line(line: &str) -> Line {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return Line::Ignore;
    };
    match v.get("type").and_then(|t| t.as_str()) {
        Some("system") if v.get("subtype").and_then(|t| t.as_str()) == Some("init") => {
            Line::Init(v)
        }
        // Streaming text lives inside stream_event → content_block_delta → text_delta.
        Some("stream_event") => v
            .get("event")
            .filter(|e| e.get("type").and_then(|t| t.as_str()) == Some("content_block_delta"))
            .and_then(text_delta)
            .map(Line::Token)
            .unwrap_or(Line::Ignore),
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
                Line::Result {
                    text,
                    usage: result_usage(&v),
                    subtype: v
                        .get("subtype")
                        .and_then(|t| t.as_str())
                        .map(str::to_string),
                }
            }
        }
        _ => Line::Ignore,
    }
}

/// The `usage` of a `result` event: prompt tokens are the fresh input plus both cache counts, since
/// all three filled the window; a missing count stays unknown.
fn result_usage(v: &serde_json::Value) -> CallUsage {
    let usage = v.get("usage");
    let count = |key: &str| usage.and_then(|u| u.get(key)).and_then(|n| n.as_u64());
    let prompt = [
        "input_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
    ]
    .iter()
    .filter_map(|k| count(k))
    .reduce(u64::saturating_add);
    CallUsage {
        prompt_tokens: prompt,
        output_tokens: count("output_tokens"),
        api_calls: 1,
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
        assert!(
            matches!(classify_line(line), Line::Result { text: Some(t), .. } if t == "final text")
        );
    }

    #[test]
    fn result_line_carries_usage_and_subtype() {
        let line = r#"{"type":"result","subtype":"success","result":"ok","usage":{"input_tokens":10,"cache_creation_input_tokens":200,"cache_read_input_tokens":3000,"output_tokens":42}}"#;
        let Line::Result { usage, subtype, .. } = classify_line(line) else {
            panic!("a result line");
        };
        assert_eq!(subtype.as_deref(), Some("success"));
        assert_eq!(usage.prompt_tokens, Some(3210));
        assert_eq!(usage.output_tokens, Some(42));
        assert_eq!(usage.api_calls, 1);

        let bare = r#"{"type":"result","result":"ok"}"#;
        let Line::Result { usage, subtype, .. } = classify_line(bare) else {
            panic!("a result line");
        };
        assert_eq!((usage.prompt_tokens, usage.output_tokens), (None, None));
        assert_eq!(subtype, None);
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
            classify_line(r#"{"type":"system","subtype":"status"}"#),
            Line::Ignore
        ));
        assert!(matches!(classify_line("not json"), Line::Ignore));
    }

    #[test]
    fn classify_init_is_kept_for_the_allowlist_check() {
        assert!(matches!(
            classify_line(r#"{"type":"system","subtype":"init","tools":["Read"]}"#),
            Line::Init(_)
        ));
    }

    #[test]
    fn flatten_single_message_is_verbatim() {
        let msgs = vec![ChatMessage {
            role: "user".into(),
            content: "just this".into(),
        }];
        assert_eq!(ClaudeCodeClient::flatten_prompt(&msgs), "just this");
    }

    fn vars(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect()
    }

    fn keys(env: &[(OsString, OsString)]) -> Vec<String> {
        env.iter()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn child_env_drops_idea_vault_and_unlisted_vars() {
        let env = child_env_from(
            vars(&[
                ("IDEA_VAULT_MCP_TOKEN", "secret"),
                ("IDEA_VAULT_OLLAMA_URL", "http://x"),
                ("AWS_SECRET_ACCESS_KEY", "aws"),
                ("GITHUB_TOKEN", "gh"),
                ("MY_EXTRA", "ok"),
                ("PATH", "/bin"),
            ]),
            // An owner listing an IDEA_VAULT_ key still cannot pass it.
            &["MY_EXTRA".to_string(), " IDEA_VAULT_MCP_TOKEN ".to_string()],
        );
        assert_eq!(keys(&env), vec!["MY_EXTRA", "PATH"]);
    }

    #[test]
    fn child_env_keeps_path_home_oauth() {
        let env = child_env_from(
            vars(&[
                ("PATH", "/usr/bin"),
                ("HOME", "/claude"),
                ("CLAUDE_CODE_OAUTH_TOKEN", "tok"),
                ("https_proxy", "http://proxy"),
                ("NODE_EXTRA_CA_CERTS", "/ca.pem"),
            ]),
            &[],
        );
        assert_eq!(
            keys(&env),
            vec![
                "PATH",
                "HOME",
                "CLAUDE_CODE_OAUTH_TOKEN",
                "https_proxy",
                "NODE_EXTRA_CA_CERTS"
            ]
        );
        assert_eq!(env[1].1, OsString::from("/claude"));
    }

    fn config(web_access: bool, mcp: Option<&str>) -> ClaudeCodeConfig {
        ClaudeCodeConfig {
            binary: "claude".into(),
            cwd: PathBuf::from("/vault/idea"),
            add_dirs: vec![PathBuf::from("/mnt/sources/refs")],
            allowed_tools: vec![
                "Bash".into(),
                "Write".into(),
                "Read".into(),
                "mcp__tracker".into(),
                "mcp__ghost".into(),
            ],
            web_access,
            model: Some("opus".into()),
            system_prompt: None,
            token_timeout: Duration::from_secs(5),
            turn_timeout: Duration::from_secs(1800),
            env_pass: Vec::new(),
            mcp_config_json: mcp.map(str::to_string),
        }
    }

    fn args_of(cfg: &ClaudeCodeConfig) -> Vec<String> {
        cfg.args(Path::new("/tmp/mcp.json"))
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn value_after(args: &[String], flag: &str) -> Option<String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1).cloned())
    }

    #[test]
    fn args_lock_the_foil_down_with_web_off() {
        let args = args_of(&config(false, None));
        assert!(args.iter().all(|a| !a.contains("dangerously")));
        assert!(args.contains(&"--restricted".to_string()));
        assert!(args.contains(&"--strict-mcp-config".to_string()));
        assert_eq!(value_after(&args, "--tools").unwrap(), "Read,Grep,Glob");
        assert_eq!(
            value_after(&args, "--allowedTools").unwrap(),
            "Read,Grep,Glob",
            "owner entries outside the tool set and unregistered MCP prefixes are dropped"
        );
        assert_eq!(
            value_after(&args, "--disallowedTools").unwrap(),
            "Read(./.runs/**),WebSearch,WebFetch"
        );
        assert_eq!(value_after(&args, "--mcp-config").unwrap(), "/tmp/mcp.json");
        assert_eq!(
            value_after(&args, "--add-dir").unwrap(),
            "/mnt/sources/refs"
        );
    }

    #[test]
    fn args_add_web_tools_and_registered_mcp_when_on() {
        let args = args_of(&config(
            true,
            Some(r#"{"mcpServers":{"tracker":{"type":"http","url":"http://t"}}}"#),
        ));
        assert_eq!(
            value_after(&args, "--tools").unwrap(),
            "Read,Grep,Glob,WebSearch,WebFetch"
        );
        assert_eq!(
            value_after(&args, "--allowedTools").unwrap(),
            "Read,Grep,Glob,WebSearch,WebFetch,mcp__tracker"
        );
        assert_eq!(
            value_after(&args, "--disallowedTools").unwrap(),
            RUN_JOURNAL_DENY,
            "the run journal stays denied with web access on"
        );
        assert!(args.contains(&"--strict-mcp-config".to_string()));
    }

    #[test]
    fn journal_deny_names_the_journal_dir() {
        // The deny must follow the journal if its directory name ever changes.
        assert_eq!(
            RUN_JOURNAL_DENY,
            format!("Read(./{}/**)", crate::ai::journal::RUNS_DIR)
        );
    }

    #[test]
    fn init_tools_accepted_are_exactly_the_allowlist() {
        let none = BTreeSet::new();
        for web in [false, true] {
            let allowed = foil_tools(web);
            let exact = serde_json::json!({"tools": allowed, "mcp_servers": []});
            assert_eq!(check_init(&exact, &allowed, &none), Ok(()));
            // Every single tool beyond the allowlist is refused, including the other web pair.
            for extra in [
                "Bash",
                "Write",
                "Edit",
                "Task",
                "WebSearch",
                "mcp__idea-vault__chat",
            ] {
                if allowed.contains(&extra) {
                    continue;
                }
                let mut tools = allowed.clone();
                tools.push(extra);
                let init = serde_json::json!({"tools": tools});
                let err = check_init(&init, &allowed, &none).unwrap_err();
                assert!(err.contains(extra), "{extra} refused: {err}");
            }
        }
        assert!(check_init(&serde_json::json!({}), FOIL_TOOLS, &none).is_err());
    }

    #[test]
    fn init_accepts_only_registered_mcp_servers() {
        let registered: BTreeSet<String> = ["tracker".to_string()].into();
        let ok = serde_json::json!({
            "tools": ["Read", "Grep", "Glob", "mcp__tracker__list"],
            "mcp_servers": [{"name": "tracker", "status": "connected"}],
        });
        assert_eq!(check_init(&ok, FOIL_TOOLS, &registered), Ok(()));
        let inherited = serde_json::json!({
            "tools": ["Read", "Grep", "Glob"],
            "mcp_servers": [{"name": "claude.ai Calendar", "status": "connected"}],
        });
        assert!(check_init(&inherited, FOIL_TOOLS, &registered).is_err());
    }
}
