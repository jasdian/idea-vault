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
}

const DEFAULT_BIND: &str = "127.0.0.1:3000";
const DEFAULT_VAULT_DIR: &str = "./vault";
const DEFAULT_INDEX_PATH: &str = "./index.db";
const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";
const DEFAULT_OLLAMA_MODEL: &str = "qwen3.5:4b";
const DEFAULT_AI_CONCURRENCY: usize = 2;
const DEFAULT_OLLAMA_TIMEOUT_SECS: u64 = 120;

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

        let vault_dir = lookup("IDEA_VAULT_VAULT_DIR")
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

        Self {
            bind,
            vault_dir,
            index_path,
            ollama_url,
            ollama_model,
            ai_concurrency,
            ollama_timeout: std::time::Duration::from_secs(ollama_timeout_secs),
        }
    }
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
