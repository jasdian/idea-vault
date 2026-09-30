//! claude-code backend tests (docs/adr/0009) against a fake `claude` CLI script that emits canned
//! `stream-json`. No real Claude, no network. Proves the streaming/parse contract; the persist
//! boundaries above the backend are identical to the Ollama path (same code in web::routes::chat).

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use idea_vault::ai::claude_code::{ClaudeCodeClient, ClaudeCodeConfig};
use idea_vault::ai::ollama::ChatMessage;
use idea_vault::ai::{AiError, AiHealth};

fn fake_claude() -> String {
    format!(
        "{}/tests/fixtures/fake-claude.sh",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// Build a client pointed at the fake, selecting its behavior via the `model` field (which the
/// fake reads from `--model`). The cwd is a fresh temp dir standing in for the idea's folder.
fn client(binary: &str, mode: Option<&str>) -> ClaudeCodeClient {
    client_with_timeout(binary, mode, Duration::from_secs(10))
}

fn client_with_timeout(
    binary: &str,
    mode: Option<&str>,
    token_timeout: Duration,
) -> ClaudeCodeClient {
    ClaudeCodeClient::new(config(
        binary,
        mode,
        token_timeout,
        Duration::from_secs(1800),
        leaked_dir(),
    ))
}

fn config(
    binary: &str,
    mode: Option<&str>,
    token_timeout: Duration,
    turn_timeout: Duration,
    cwd: PathBuf,
) -> ClaudeCodeConfig {
    ClaudeCodeConfig {
        binary: binary.to_string(),
        cwd,
        add_dirs: Vec::new(),
        allowed_tools: Vec::new(),
        web_access: false,
        model: mode.map(str::to_string),
        system_prompt: None,
        token_timeout,
        turn_timeout,
        env_pass: Vec::new(),
        mcp_config_json: None,
    }
}

/// A temp dir kept for the process lifetime (the fake writes its recordings into its cwd).
fn leaked_dir() -> PathBuf {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    std::mem::forget(dir);
    path
}

/// A client over the fake in `mode`, running in `cwd` with the given turn deadline.
fn client_in(
    mode: &str,
    cwd: &std::path::Path,
    token_timeout: Duration,
    turn: Duration,
) -> ClaudeCodeClient {
    ClaudeCodeClient::new(config(
        &fake_claude(),
        Some(mode),
        token_timeout,
        turn,
        cwd.to_path_buf(),
    ))
}

fn msg(text: &str) -> Vec<ChatMessage> {
    vec![ChatMessage {
        role: "user".into(),
        content: text.into(),
    }]
}

#[tokio::test]
async fn streams_text_deltas_and_ignores_tool_events() {
    let c = client(&fake_claude(), Some("tokens"));
    let mut stream = c.chat_stream(msg("hi")).await.unwrap();
    let mut tokens = Vec::new();
    while let Some(item) = stream.next().await {
        tokens.push(item.expect("clean stream has no errors"));
    }
    // The tool_use and system/init lines are consumed silently; only prose deltas surface.
    assert_eq!(tokens, ["Hello ", "world"]);
}

#[tokio::test]
async fn chat_concatenates_the_stream() {
    let c = client(&fake_claude(), Some("tokens"));
    assert_eq!(c.chat(msg("hi")).await.unwrap(), "Hello world");
}

#[tokio::test]
async fn result_only_falls_back_to_result_text() {
    // No streaming deltas, just a terminal result — the text must not be lost.
    let c = client(&fake_claude(), Some("resulttext"));
    assert_eq!(c.chat(msg("hi")).await.unwrap(), "whole answer");
}

#[tokio::test]
async fn eof_before_result_is_a_terminal_backend_error() {
    // The fake streams one token then exits without a `result` — the partial must surface an error
    // (so the caller persists nothing), not a clean end.
    let c = client(&fake_claude(), Some("eof"));
    let mut stream = c.chat_stream(msg("hi")).await.unwrap();
    assert_eq!(stream.next().await.unwrap().unwrap(), "partial");
    assert!(matches!(
        stream.next().await.unwrap().unwrap_err(),
        AiError::Backend(_)
    ));
    assert!(stream.next().await.is_none(), "error is terminal");
}

#[tokio::test]
async fn auth_failure_surfaces_as_backend_error() {
    let c = client(&fake_claude(), Some("auth"));
    match c.chat(msg("hi")).await {
        Err(AiError::Backend(detail)) => assert!(detail.contains("401")),
        other => panic!("expected auth backend error, got {other:?}"),
    }
}

#[tokio::test]
async fn probe_available_when_binary_runs_unreachable_otherwise() {
    assert_eq!(
        client(&fake_claude(), None).probe().await,
        AiHealth::Available
    );
    assert_eq!(
        client("/nonexistent/claude-binary", None).probe().await,
        AiHealth::Unreachable
    );
}

#[tokio::test]
async fn spawn_failure_is_a_backend_error() {
    let c = client("/nonexistent/claude-binary", Some("tokens"));
    match c.chat_stream(msg("hi")).await {
        Err(AiError::Backend(_)) => {}
        Err(other) => panic!("expected spawn Backend error, got {other:?}"),
        Ok(_) => panic!("expected spawn to fail"),
    }
}

#[tokio::test]
async fn prompt_write_to_a_cli_that_never_reads_stdin_times_out() {
    // A prompt larger than the pipe buffer blocks the write until the CLI reads; it never does.
    let c = client_with_timeout(
        &fake_claude(),
        Some("stalledstdin"),
        Duration::from_millis(500),
    );
    let prompt = "x".repeat(1 << 20);
    let outcome = tokio::time::timeout(Duration::from_secs(10), c.chat_stream(msg(&prompt)))
        .await
        .expect("the stdin write must be bounded by the token timeout, not hang");
    match outcome {
        Err(AiError::Timeout) => {}
        Err(other) => panic!("expected Timeout, got {other:?}"),
        Ok(_) => panic!("expected the prompt write to time out"),
    }
}

const LONG: Duration = Duration::from_secs(1800);

#[tokio::test]
async fn claude_child_does_not_see_mcp_token() {
    // The server holds the inbound MCP token in its own environment (config.rs); the foil must
    // not inherit it, nor anything else outside the pass-list (ADR-0039).
    std::env::set_var("IDEA_VAULT_MCP_TOKEN", "leak-marker-7f3a");
    std::env::set_var("SOME_UNLISTED_SECRET", "leak-marker-91c2");
    let dir = leaked_dir();
    let c = client_in("dumpenv", &dir, Duration::from_secs(10), LONG);
    assert_eq!(c.chat(msg("hi")).await.unwrap(), "recorded");
    let env = std::fs::read_to_string(dir.join("fake-claude.env")).unwrap();
    assert!(!env.contains("leak-marker"), "child env leaked: {env}");
    assert!(
        !env.contains("IDEA_VAULT_"),
        "no app key reaches the child: {env}"
    );
    assert!(
        env.lines().any(|l| l.starts_with("PATH=")),
        "PATH passes: {env}"
    );
}

#[tokio::test]
async fn claude_argv_is_the_locked_down_foil() {
    let dir = leaked_dir();
    let c = client_in("dumpenv", &dir, Duration::from_secs(10), LONG);
    c.chat(msg("hi")).await.unwrap();
    let argv: Vec<String> = std::fs::read_to_string(dir.join("fake-claude.argv"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    let after = |flag: &str| {
        argv.iter()
            .position(|a| a == flag)
            .map(|i| argv[i + 1].clone())
            .unwrap_or_else(|| panic!("{flag} missing from {argv:?}"))
    };
    assert!(
        argv.iter().all(|a| !a.contains("dangerously")),
        "never skips permissions: {argv:?}"
    );
    assert!(argv.iter().any(|a| a == "--restricted"), "{argv:?}");
    assert!(argv.iter().any(|a| a == "--strict-mcp-config"), "{argv:?}");
    assert_eq!(after("--tools"), "Read,Grep,Glob");
    assert_eq!(after("--allowedTools"), "Read,Grep,Glob");
    assert_eq!(
        after("--disallowedTools"),
        "Read(./.runs/**),WebSearch,WebFetch"
    );
    // Always an MCP config file, even with no server registered; removed once the turn ends.
    let mcp_path = after("--mcp-config");
    assert!(
        !std::path::Path::new(&mcp_path).exists(),
        "the per-call mcp config is removed after the turn"
    );
}

#[tokio::test]
async fn claude_empty_mcp_config_backs_strict_mode() {
    // busytools never ends on its own, so the config can be read while the turn is open.
    let dir = leaked_dir();
    let c = client_in(
        "busytools",
        &dir,
        Duration::from_secs(5),
        Duration::from_secs(30),
    );
    let stream = c.chat_stream(msg("hi")).await.unwrap();
    let prefix = format!("idea-vault-mcp-{}-", std::process::id());
    let empty = std::fs::read_dir(std::env::temp_dir())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&prefix))
        })
        .any(|p| std::fs::read_to_string(p).is_ok_and(|s| s == r#"{"mcpServers":{}}"#));
    drop(stream);
    assert!(
        empty,
        "an empty mcpServers config backs --strict-mcp-config"
    );
}

#[tokio::test]
async fn init_tools_outside_the_allowlist_refuse_the_turn() {
    let c = client(&fake_claude(), Some("leakytools"));
    match c.chat(msg("hi")).await {
        Err(AiError::Backend(detail)) => {
            assert!(detail.contains("Bash"), "names the leaked tool: {detail}")
        }
        other => panic!("expected the lockdown to refuse, got {other:?}"),
    }
}

#[tokio::test]
async fn output_before_the_init_event_is_refused() {
    let c = client(&fake_claude(), Some("noinit"));
    match c.chat(msg("hi")).await {
        Err(AiError::Backend(detail)) => assert!(detail.contains("init"), "{detail}"),
        other => panic!("expected an unverified session to be refused, got {other:?}"),
    }
}

/// Is `pid` still a live (non-zombie) process? A zombie is dead, only not yet reaped.
fn alive(pid: &str) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map(|stat| {
            let state = stat.rsplit(')').next().unwrap_or("").trim_start();
            !state.starts_with('Z')
        })
        .unwrap_or(false)
}

#[tokio::test]
async fn busy_tool_events_hit_turn_deadline() {
    let dir = leaked_dir();
    let c = client_in(
        "busytools",
        &dir,
        Duration::from_secs(5),
        Duration::from_millis(500),
    );
    let started = std::time::Instant::now();
    let outcome = c.chat(msg("hi")).await;
    let took = started.elapsed();
    match outcome {
        Err(AiError::Backend(detail)) => {
            assert!(detail.contains("wall clock"), "deadline error: {detail}")
        }
        other => panic!("expected the turn deadline, got {other:?}"),
    }
    assert!(took < Duration::from_secs(2), "ended in {took:?}");

    // The process is killed once the stream drops (kill_on_drop).
    let pid = std::fs::read_to_string(dir.join("fake-claude.pid"))
        .unwrap()
        .trim()
        .to_string();
    let gone = async {
        while alive(&pid) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(2), gone)
        .await
        .expect("the fake claude process is killed after the deadline");
}
