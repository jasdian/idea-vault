//! Environment-driven configuration — docs/12-deployment.md "Configuration contract" table,
//! ADR-0006 (bounded concurrency), ADR-0008 (containerized deployment).
//!
//! Every default here is the **bare `cargo run`** default (loopback bind, `./vault`,
//! `./index.db`, `http://localhost:11434`). Compose overrides all of these via env vars — see
//! the table in docs/12-deployment.md — so nothing here may hardcode a container-only value and
//! nothing outside `config.rs` may hardcode `localhost:11434` or a bind address.

use std::path::PathBuf;

/// Resolved runtime configuration for the whole crate.
///
/// Constructed once at boot ([D25](../docs/01-architecture.md)) and passed down as shared state;
/// nothing downstream should read env vars directly.
#[derive(Debug, Clone)]
pub struct Config {
    /// axum bind address, e.g. `127.0.0.1:3000` (bare run) or `0.0.0.0:3000` (in-container).
    pub bind: String,
    /// Vault root directory — markdown source of truth (docs/03-data-model.md).
    pub vault_dir: PathBuf,
    /// SQLite index file path — rebuildable via `reindex` (ADR-0002).
    pub index_path: PathBuf,
    /// Ollama base URL, trailing slash trimmed. Never hardcode this elsewhere.
    pub ollama_url: String,
    /// Default Ollama model tag, shared with the `ollama-pull` one-shot (docs/12-deployment.md).
    pub ollama_model: String,
    /// Process-wide swarm concurrency bound (ADR-0006): a single semaphore of this size gates
    /// how many subagent tasks may call Ollama at once; excess tasks queue rather than run, so
    /// a local, single-server Ollama instance stays responsive under fan-out.
    pub ai_concurrency: usize,
    /// Hard inactivity timeout for Ollama calls (D20): the initial response and every token gap
    /// must arrive within this window or the call aborts (docs/05 "per-request timeout").
    pub ollama_timeout: std::time::Duration,
    /// Ollama sampling temperature (initial value; the Settings page can retune it live).
    pub ollama_temperature: f32,
    /// Which LLM backend answers chat/skills/swarm at boot (docs/adr/0009). The Settings page can
    /// toggle this live without a restart.
    pub llm_backend: LlmBackendKind,
    /// claude-code backend settings (both backends are always constructed so the toggle is live).
    pub claude: ClaudeSettings,
    /// Auto-compact enabled at boot (docs/adr/0012). Live-retunable on the Settings page. Defaults
    /// on: with silent-drop as the pre-auto-compact failure mode, folding on is the safer default.
    pub auto_compact: bool,
    /// The effective-size fraction of the AI budget at which auto-compact fires (default 0.80,
    /// clamped 0.5..=0.95).
    pub compact_threshold: f32,
    /// Initial Ollama context-window override in tokens; `0` (default) = auto — derive from the
    /// model via `/api/show`. Nonzero values are clamped to 1024..=2_000_000. Initial value only:
    /// the Settings page retunes it live (ADR-0011).
    pub ollama_ctx_tokens: usize,
    /// Initial claude-code context-window override in tokens; `0` (default) = auto — derive from
    /// the model name. Nonzero values are clamped to 1024..=2_000_000. Initial value only.
    pub claude_ctx_tokens: usize,
}

/// The selectable LLM backend (docs/adr/0009). Defaults to Ollama for an offline local run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmBackendKind {
    Ollama,
    ClaudeCode,
}

/// Settings for the claude-code backend. `cwd` deliberately defaults to the vault dir (never the
/// idea-vault source tree) so a full-agentic foil cannot rewrite the app itself.
#[derive(Debug, Clone)]
pub struct ClaudeSettings {
    pub binary: String,
    pub cwd: PathBuf,
    pub add_dirs: Vec<PathBuf>,
    pub allowed_tools: Vec<String>,
    pub model: Option<String>,
    pub skip_permissions: bool,
    pub timeout: std::time::Duration,
    /// Reasoning effort (`low`/`medium`/`high`) — initial value, retunable on the Settings page.
    pub effort: String,
}

const DEFAULT_BIND: &str = "127.0.0.1:3000";
const DEFAULT_VAULT_DIR: &str = "./vault";
const DEFAULT_INDEX_PATH: &str = "./index.db";
const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";
const DEFAULT_OLLAMA_MODEL: &str = "qwen3.5:4b";
const DEFAULT_AI_CONCURRENCY: usize = 2;
const DEFAULT_OLLAMA_TIMEOUT_SECS: u64 = 120;
const DEFAULT_CLAUDE_BIN: &str = "claude";
// Agentic turns (process spawn + tool use) run longer than a hot local model.
const DEFAULT_CLAUDE_TIMEOUT_SECS: u64 = 300;
const DEFAULT_OLLAMA_TEMPERATURE: f32 = 0.7;
const DEFAULT_CLAUDE_EFFORT: &str = "high";
const DEFAULT_AUTO_COMPACT: bool = true;
const DEFAULT_COMPACT_THRESHOLD: f32 = 0.80;
/// Clamp band for a nonzero context-window override (tokens): below 1k is useless, above 2M is
/// beyond any supported model (the claude 1M window fits comfortably).
pub const CTX_TOKENS_MIN: usize = 1_024;
pub const CTX_TOKENS_MAX: usize = 2_000_000;

impl Config {
    /// Build configuration from the real process environment.
    pub fn from_env() -> Self {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Pure construction path: `lookup` maps an env var name to its value, if set. Kept separate
    /// from [`Config::from_env`] so tests can exercise defaults and overrides without mutating
    /// real process environment state.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let bind = lookup("IDEA_VAULT_BIND").unwrap_or_else(|| DEFAULT_BIND.to_string());

        let vault_dir: PathBuf = lookup("IDEA_VAULT_VAULT_DIR")
            .unwrap_or_else(|| DEFAULT_VAULT_DIR.to_string())
            .into();

        let index_path = lookup("IDEA_VAULT_INDEX_PATH")
            .unwrap_or_else(|| DEFAULT_INDEX_PATH.to_string())
            .into();

        let ollama_url = lookup("IDEA_VAULT_OLLAMA_URL")
            .unwrap_or_else(|| DEFAULT_OLLAMA_URL.to_string())
            .trim_end_matches('/')
            .to_string();

        let ollama_model =
            lookup("IDEA_VAULT_OLLAMA_MODEL").unwrap_or_else(|| DEFAULT_OLLAMA_MODEL.to_string());

        let ai_concurrency = match lookup("IDEA_VAULT_AI_CONCURRENCY") {
            None => DEFAULT_AI_CONCURRENCY,
            Some(raw) => raw.parse::<usize>().unwrap_or_else(|_| {
                tracing::warn!(
                    value = %raw,
                    default = DEFAULT_AI_CONCURRENCY,
                    "IDEA_VAULT_AI_CONCURRENCY unparsable as usize, falling back to default"
                );
                DEFAULT_AI_CONCURRENCY
            }),
        };

        let ollama_timeout_secs = match lookup("IDEA_VAULT_OLLAMA_TIMEOUT_SECS") {
            None => DEFAULT_OLLAMA_TIMEOUT_SECS,
            Some(raw) => raw.parse::<u64>().unwrap_or_else(|_| {
                tracing::warn!(
                    value = %raw,
                    default = DEFAULT_OLLAMA_TIMEOUT_SECS,
                    "IDEA_VAULT_OLLAMA_TIMEOUT_SECS unparsable as u64, falling back to default"
                );
                DEFAULT_OLLAMA_TIMEOUT_SECS
            }),
        };

        let llm_backend = match lookup("IDEA_VAULT_LLM_BACKEND").as_deref() {
            Some("claude-code") | Some("claude") => LlmBackendKind::ClaudeCode,
            Some("ollama") | None => LlmBackendKind::Ollama,
            Some(other) => {
                tracing::warn!(value = %other, "unknown IDEA_VAULT_LLM_BACKEND, using ollama");
                LlmBackendKind::Ollama
            }
        };

        let claude_timeout_secs = match lookup("IDEA_VAULT_CLAUDE_TIMEOUT_SECS") {
            None => DEFAULT_CLAUDE_TIMEOUT_SECS,
            Some(raw) => raw.parse::<u64>().unwrap_or(DEFAULT_CLAUDE_TIMEOUT_SECS),
        };

        let ollama_temperature = lookup("IDEA_VAULT_OLLAMA_TEMPERATURE")
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|t| (0.0..=2.0).contains(t))
            .unwrap_or(DEFAULT_OLLAMA_TEMPERATURE);
        let claude_effort = lookup("IDEA_VAULT_CLAUDE_EFFORT")
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_CLAUDE_EFFORT.to_string());

        // Auto-compact (docs/adr/0012): on unless explicitly `false`/`0`.
        let auto_compact = lookup("IDEA_VAULT_AUTO_COMPACT")
            .map(|v| v != "false" && v != "0")
            .unwrap_or(DEFAULT_AUTO_COMPACT);
        // Threshold fraction of the AI budget; out-of-range or unparsable falls back to default.
        let compact_threshold = lookup("IDEA_VAULT_COMPACT_THRESHOLD")
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|t| (0.5..=0.95).contains(t))
            .unwrap_or(DEFAULT_COMPACT_THRESHOLD);

        // Per-backend context-window overrides (dynamic budget): 0 = auto; nonzero clamped.
        let ollama_ctx_tokens = parse_ctx_tokens(&lookup, "IDEA_VAULT_OLLAMA_CTX_TOKENS");
        let claude_ctx_tokens = parse_ctx_tokens(&lookup, "IDEA_VAULT_CLAUDE_CTX_TOKENS");

        let claude = ClaudeSettings {
            binary: lookup("IDEA_VAULT_CLAUDE_BIN")
                .unwrap_or_else(|| DEFAULT_CLAUDE_BIN.to_string()),
            // Default the foil's working dir to the vault (owner content), never the app source.
            cwd: lookup("IDEA_VAULT_CLAUDE_CWD")
                .map(PathBuf::from)
                .unwrap_or_else(|| vault_dir.clone()),
            add_dirs: split_paths(lookup("IDEA_VAULT_CLAUDE_ADD_DIRS")),
            allowed_tools: split_csv(lookup("IDEA_VAULT_CLAUDE_ALLOWED_TOOLS")),
            model: lookup("IDEA_VAULT_CLAUDE_MODEL").filter(|s| !s.trim().is_empty()),
            // Unattended server: default to skipping interactive permission prompts (the owner's
            // "full agentic" choice; see docs/adr/0009). Set to `false` for a locked-down run.
            skip_permissions: lookup("IDEA_VAULT_CLAUDE_SKIP_PERMISSIONS")
                .map(|v| v != "false" && v != "0")
                .unwrap_or(true),
            timeout: std::time::Duration::from_secs(claude_timeout_secs),
            effort: claude_effort,
        };

        Self {
            bind,
            vault_dir,
            index_path,
            ollama_url,
            ollama_model,
            ai_concurrency,
            ollama_timeout: std::time::Duration::from_secs(ollama_timeout_secs),
            ollama_temperature,
            llm_backend,
            claude,
            auto_compact,
            compact_threshold,
            ollama_ctx_tokens,
            claude_ctx_tokens,
        }
    }
}

/// Parse a context-window override env var: unset or `0` = auto (`0`); unparsable = auto with a
/// warning; any other value is clamped into the supported 1024..=2_000_000 token band.
fn parse_ctx_tokens(lookup: &impl Fn(&str) -> Option<String>, key: &str) -> usize {
    match lookup(key) {
        None => 0,
        Some(raw) => match raw.trim().parse::<usize>() {
            Ok(0) => 0,
            Ok(n) => n.clamp(CTX_TOKENS_MIN, CTX_TOKENS_MAX),
            Err(_) => {
                tracing::warn!(
                    value = %raw,
                    key,
                    "context-tokens override unparsable as usize, falling back to 0 (auto)"
                );
                0
            }
        },
    }
}

/// Split a colon-separated `PATH`-style list into paths, dropping empties.
fn split_paths(raw: Option<String>) -> Vec<PathBuf> {
    raw.into_iter()
        .flat_map(|s| {
            s.split(':')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(PathBuf::from)
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Split a comma-separated list into trimmed, non-empty items.
fn split_csv(raw: Option<String>) -> Vec<String> {
    raw.into_iter()
        .flat_map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup_from(map: HashMap<&'static str, &'static str>) -> impl Fn(&str) -> Option<String> {
        move |key| map.get(key).map(|v| v.to_string())
    }

    #[test]
    fn defaults_when_nothing_set() {
        let cfg = Config::from_lookup(lookup_from(HashMap::new()));

        assert_eq!(cfg.bind, "127.0.0.1:3000");
        assert_eq!(cfg.vault_dir, PathBuf::from("./vault"));
        assert_eq!(cfg.index_path, PathBuf::from("./index.db"));
        assert_eq!(cfg.ollama_url, "http://localhost:11434");
        assert_eq!(cfg.ollama_model, "qwen3.5:4b");
        assert_eq!(cfg.ai_concurrency, 2);
        assert_eq!(cfg.ollama_timeout, std::time::Duration::from_secs(120));
        // Backend defaults to Ollama for an offline local run; claude settings are sane.
        assert_eq!(cfg.llm_backend, LlmBackendKind::Ollama);
        assert_eq!(cfg.claude.binary, "claude");
        assert_eq!(
            cfg.claude.cwd, cfg.vault_dir,
            "claude cwd defaults to the vault, not the app"
        );
        assert!(cfg.claude.add_dirs.is_empty());
        assert!(cfg.claude.skip_permissions);
        assert_eq!(cfg.claude.timeout, std::time::Duration::from_secs(300));
        // Auto-compact defaults on, threshold 0.80.
        assert!(cfg.auto_compact);
        assert_eq!(cfg.compact_threshold, 0.80);
        // Context windows default to auto (0).
        assert_eq!(cfg.ollama_ctx_tokens, 0);
        assert_eq!(cfg.claude_ctx_tokens, 0);
    }

    #[test]
    fn ctx_tokens_override_zero_clamp_and_fallback() {
        // Explicit 0 stays auto; a plain value passes through.
        let mut map = HashMap::new();
        map.insert("IDEA_VAULT_OLLAMA_CTX_TOKENS", "0");
        map.insert("IDEA_VAULT_CLAUDE_CTX_TOKENS", "64000");
        let cfg = Config::from_lookup(lookup_from(map));
        assert_eq!(cfg.ollama_ctx_tokens, 0);
        assert_eq!(cfg.claude_ctx_tokens, 64_000);

        // Nonzero values are clamped into 1024..=2_000_000.
        let mut map = HashMap::new();
        map.insert("IDEA_VAULT_OLLAMA_CTX_TOKENS", "12");
        map.insert("IDEA_VAULT_CLAUDE_CTX_TOKENS", "9999999");
        let cfg = Config::from_lookup(lookup_from(map));
        assert_eq!(cfg.ollama_ctx_tokens, CTX_TOKENS_MIN);
        assert_eq!(cfg.claude_ctx_tokens, CTX_TOKENS_MAX);

        // Unparsable falls back to auto (0), not a panic.
        let mut map = HashMap::new();
        map.insert("IDEA_VAULT_OLLAMA_CTX_TOKENS", "lots");
        let cfg = Config::from_lookup(lookup_from(map));
        assert_eq!(cfg.ollama_ctx_tokens, 0);
    }

    #[test]
    fn auto_compact_and_threshold_override_and_clamp() {
        // Explicit disable + in-range threshold.
        let mut map = HashMap::new();
        map.insert("IDEA_VAULT_AUTO_COMPACT", "false");
        map.insert("IDEA_VAULT_COMPACT_THRESHOLD", "0.9");
        let cfg = Config::from_lookup(lookup_from(map));
        assert!(!cfg.auto_compact);
        assert_eq!(cfg.compact_threshold, 0.9);

        // Out-of-range threshold falls back to default; "0" also disables auto-compact.
        let mut map = HashMap::new();
        map.insert("IDEA_VAULT_AUTO_COMPACT", "0");
        map.insert("IDEA_VAULT_COMPACT_THRESHOLD", "0.2");
        let cfg = Config::from_lookup(lookup_from(map));
        assert!(!cfg.auto_compact);
        assert_eq!(cfg.compact_threshold, DEFAULT_COMPACT_THRESHOLD);

        // Unparsable threshold falls back too; any other AUTO_COMPACT value keeps it on.
        let mut map = HashMap::new();
        map.insert("IDEA_VAULT_AUTO_COMPACT", "yes");
        map.insert("IDEA_VAULT_COMPACT_THRESHOLD", "loads");
        let cfg = Config::from_lookup(lookup_from(map));
        assert!(cfg.auto_compact);
        assert_eq!(cfg.compact_threshold, DEFAULT_COMPACT_THRESHOLD);
    }

    #[test]
    fn claude_backend_selection_and_lists() {
        let mut map = HashMap::new();
        map.insert("IDEA_VAULT_LLM_BACKEND", "claude-code");
        map.insert("IDEA_VAULT_CLAUDE_BIN", "/usr/bin/claude");
        map.insert(
            "IDEA_VAULT_CLAUDE_ADD_DIRS",
            "/home/x/vault:/home/x/artifacts",
        );
        map.insert("IDEA_VAULT_CLAUDE_ALLOWED_TOOLS", "Read, Grep , Glob");
        map.insert("IDEA_VAULT_CLAUDE_MODEL", "opus");
        map.insert("IDEA_VAULT_CLAUDE_SKIP_PERMISSIONS", "false");
        let cfg = Config::from_lookup(lookup_from(map));

        assert_eq!(cfg.llm_backend, LlmBackendKind::ClaudeCode);
        assert_eq!(cfg.claude.binary, "/usr/bin/claude");
        assert_eq!(
            cfg.claude.add_dirs,
            vec![
                PathBuf::from("/home/x/vault"),
                PathBuf::from("/home/x/artifacts")
            ]
        );
        assert_eq!(cfg.claude.allowed_tools, vec!["Read", "Grep", "Glob"]);
        assert_eq!(cfg.claude.model.as_deref(), Some("opus"));
        assert!(!cfg.claude.skip_permissions);

        // Alias + unknown fallback.
        let alias = Config::from_lookup(lookup_from(HashMap::from([(
            "IDEA_VAULT_LLM_BACKEND",
            "claude",
        )])));
        assert_eq!(alias.llm_backend, LlmBackendKind::ClaudeCode);
        let bogus = Config::from_lookup(lookup_from(HashMap::from([(
            "IDEA_VAULT_LLM_BACKEND",
            "gpt5",
        )])));
        assert_eq!(bogus.llm_backend, LlmBackendKind::Ollama);
    }

    #[test]
    fn ollama_timeout_override_and_fallback() {
        let mut map = HashMap::new();
        map.insert("IDEA_VAULT_OLLAMA_TIMEOUT_SECS", "300");
        let cfg = Config::from_lookup(lookup_from(map));
        assert_eq!(cfg.ollama_timeout, std::time::Duration::from_secs(300));

        let mut map = HashMap::new();
        map.insert("IDEA_VAULT_OLLAMA_TIMEOUT_SECS", "soon");
        let cfg = Config::from_lookup(lookup_from(map));
        assert_eq!(cfg.ollama_timeout, std::time::Duration::from_secs(120));
    }

    #[test]
    fn full_override_map() {
        let mut map = HashMap::new();
        map.insert("IDEA_VAULT_BIND", "0.0.0.0:3000");
        map.insert("IDEA_VAULT_VAULT_DIR", "/vault");
        map.insert("IDEA_VAULT_INDEX_PATH", "/data/index.db");
        map.insert("IDEA_VAULT_OLLAMA_URL", "http://ollama:11434");
        map.insert("IDEA_VAULT_OLLAMA_MODEL", "mistral");
        map.insert("IDEA_VAULT_AI_CONCURRENCY", "5");

        let cfg = Config::from_lookup(lookup_from(map));

        assert_eq!(cfg.bind, "0.0.0.0:3000");
        assert_eq!(cfg.vault_dir, PathBuf::from("/vault"));
        assert_eq!(cfg.index_path, PathBuf::from("/data/index.db"));
        assert_eq!(cfg.ollama_url, "http://ollama:11434");
        assert_eq!(cfg.ollama_model, "mistral");
        assert_eq!(cfg.ai_concurrency, 5);
    }

    #[test]
    fn trims_trailing_slash_from_ollama_url() {
        let mut map = HashMap::new();
        map.insert("IDEA_VAULT_OLLAMA_URL", "http://ollama:11434/");
        let cfg = Config::from_lookup(lookup_from(map));
        assert_eq!(cfg.ollama_url, "http://ollama:11434");
    }

    #[test]
    fn unparsable_concurrency_falls_back_to_default() {
        let mut map = HashMap::new();
        map.insert("IDEA_VAULT_AI_CONCURRENCY", "not-a-number");
        let cfg = Config::from_lookup(lookup_from(map));
        assert_eq!(cfg.ai_concurrency, DEFAULT_AI_CONCURRENCY);
    }
}
