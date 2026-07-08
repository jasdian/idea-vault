//! Persistent MCP server registry — the owner's list of MCP (Model Context Protocol) endpoints,
//! stored as one JSON file (`config.mcp_config_path`, default `<vault>/.mcp-servers.json`).
//!
//! **This is app config, NOT vault truth.** The dotfile lives beside the ideas only because the
//! vault bind mount is the one host-persistent path in a containerized run — it must never enter
//! the SQLite index (`vault::walk::walk_ideas` only admits *directories* containing an `idea.md`,
//! so a top-level dotfile is invisible to reindex by construction) and losing it costs the owner
//! a re-add of server URLs, not ideas.
//!
//! **Deliberate dependency split (cycle avoidance):** this module is pure config/persistence —
//! `std` + `serde` only. The MCP *wire client* lives in [`ai::mcp`](crate::ai::mcp), and the
//! *bridge* that combines "which servers are enabled" (here) with "connect and call their tools"
//! (`ai::mcp`) is `ai::backend`'s tool loop. `mcp` must never import `ai`, and `ai` reaches this
//! registry only through the `Option<Arc<McpRegistry>>` handed to `LlmBackend::with_mcp` — a
//! one-way `ai → mcp` edge, no cycle.
//!
//! Failure discipline matches the rest of the crate: a missing file is an empty registry, an
//! unparsable file is a warning + empty registry (boot must never crash on owner-editable JSON),
//! and every mutation persists via a same-directory tmp+rename so a crash mid-save can never
//! leave a half-written file. The atomic-write helper is implemented locally rather than reused
//! from `vault::store` — depending on `vault` here would put app config inside the truth module.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

/// One configured MCP server endpoint. `name` doubles as the tool-name prefix on both backends
/// (`mcp__<name>__<tool>` in the Ollama loop, `mcp__<name>` in the claude CLI allowlist), so it
/// is restricted to a slug alphabet that survives tool-name mangling: `[a-z0-9-]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    /// The server's Streamable-HTTP endpoint. Owner-supplied — never assumed to be localhost.
    pub url: String,
    /// Sent as `Authorization: Bearer <token>` when present; omitted entirely otherwise.
    pub bearer_token: Option<String>,
    /// Disabled servers stay in the file (the owner keeps the URL/token) but are invisible to
    /// [`McpRegistry::enabled`] — the only accessor the backends consult.
    pub enabled: bool,
}

/// How the owner wants `update` to change a server's stored bearer token. The token is
/// write-only from the browser (`GET /mcp/{name}/edit` never echoes it back), so a blank form
/// field is genuinely ambiguous between "leave it alone" and "I have nothing to type" — the edit
/// form disambiguates with an explicit "clear token" checkbox, and this enum carries that
/// three-way decision down to the registry instead of overloading `Option<String>` (where `None`
/// would be unable to distinguish "keep" from "clear").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenChange {
    /// Blank token field, checkbox unticked — the stored token (if any) is untouched.
    Keep,
    /// "Clear token" ticked — erase the stored token regardless of the text field.
    Clear,
    /// A non-blank token field — replace the stored token with this value.
    Set(String),
}

/// Validate a server name: non-empty `[a-z0-9-]`. Kept strict because the name is spliced into
/// model-facing tool names — an `_` would collide with the `__` separators, spaces/uppercase
/// would break the claude CLI's `mcp__<name>` allow-prefix convention.
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The registry: an in-memory server list mirrored to `path` after every mutation. `Arc`'d into
/// `AppState` and (optionally) into `ai::LlmBackend`, so a Settings-page edit is visible to the
/// very next model turn with no restart — same live-tuning discipline as `LlmSettings`.
pub struct McpRegistry {
    path: PathBuf,
    servers: RwLock<Vec<McpServerConfig>>,
    /// Last-known serialized size (bytes) of each server's tool definitions, keyed by name.
    /// In-memory only (never persisted): it is a display cache for the usage meter — the tool
    /// schemas ride every model turn and the meter must not pretend they are free. Populated by
    /// whatever last listed the tools (a probe, or a turn's bridge), so it can be a little stale;
    /// stale-but-honest beats a per-render network call.
    tools_bytes: RwLock<HashMap<String, usize>>,
}

impl McpRegistry {
    /// Load the registry from `path` at boot. Missing file ⇒ empty list; unreadable/unparsable
    /// file ⇒ `tracing::warn` + empty list — a corrupt config file must never crash boot (the
    /// owner re-adds servers on the Settings page; the broken file is only overwritten on the
    /// next mutation, so it stays inspectable until then).
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let servers = match std::fs::read_to_string(&path) {
            Ok(raw) => match serde_json::from_str::<Vec<McpServerConfig>>(&raw) {
                Ok(list) => list,
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "mcp server config unparsable; starting with an empty registry"
                    );
                    Vec::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "mcp server config unreadable; starting with an empty registry"
                );
                Vec::new()
            }
        };
        Self {
            path,
            servers: RwLock::new(servers),
            tools_bytes: RwLock::new(HashMap::new()),
        }
    }

    /// Record the serialized size of one server's tool definitions (from a probe or a turn's
    /// listing) for the usage meter.
    pub fn note_tools_bytes(&self, name: &str, bytes: usize) {
        self.tools_bytes
            .write()
            .expect("mcp tools-bytes lock poisoned")
            .insert(name.to_string(), bytes);
    }

    /// Sum of the last-known tool-definition sizes across *enabled* servers — the meter's
    /// "(+N KB tools)" term. Servers never yet listed contribute 0 (unknown ≠ invented).
    pub fn enabled_tools_bytes(&self) -> usize {
        // Lock order everywhere both are held: `servers` first, then `tools_bytes` (remove()
        // relies on the same order — mixed orders would be a deadlock waiting for load).
        let servers = self.read_lock();
        let sizes = self
            .tools_bytes
            .read()
            .expect("mcp tools-bytes lock poisoned");
        servers
            .iter()
            .filter(|s| s.enabled)
            .filter_map(|s| sizes.get(&s.name))
            .sum()
    }

    /// Add a server: reject an invalid name, a non-http(s) URL, or a duplicate name (names are
    /// the routing key for `mcp__<name>__<tool>` calls — two servers under one name would be
    /// indistinguishable). Persists on success.
    pub fn add(&self, cfg: McpServerConfig) -> Result<(), String> {
        if !is_valid_name(&cfg.name) {
            return Err(format!(
                "invalid server name '{}': use lowercase letters, digits and '-' only",
                cfg.name
            ));
        }
        if !(cfg.url.starts_with("http://") || cfg.url.starts_with("https://")) {
            return Err(format!(
                "invalid server url '{}': must start with http:// or https://",
                cfg.url
            ));
        }
        {
            let mut servers = self.write_lock();
            if servers.iter().any(|s| s.name == cfg.name) {
                return Err(format!("a server named '{}' already exists", cfg.name));
            }
            servers.push(cfg);
        }
        self.save()
    }

    /// Toggle one server. Errs on an unknown name so a stale Settings form gets a readable
    /// failure instead of silently doing nothing. Persists on success.
    pub fn set_enabled(&self, name: &str, enabled: bool) -> Result<(), String> {
        {
            let mut servers = self.write_lock();
            let Some(server) = servers.iter_mut().find(|s| s.name == name) else {
                return Err(format!("no mcp server named '{name}'"));
            };
            server.enabled = enabled;
        }
        self.save()
    }

    /// Remove one server (URL and token gone from disk too). Errs on an unknown name. Persists
    /// on success.
    pub fn remove(&self, name: &str) -> Result<(), String> {
        {
            // Both maps mutate under ONE servers-lock critical section (servers → tools_bytes,
            // the crate-wide order) so a concurrent re-add of the same name can never have its
            // freshly-noted tool size swept away by this removal's cleanup.
            let mut servers = self.write_lock();
            let before = servers.len();
            servers.retain(|s| s.name != name);
            if servers.len() == before {
                return Err(format!("no mcp server named '{name}'"));
            }
            self.tools_bytes
                .write()
                .expect("mcp tools-bytes lock poisoned")
                .remove(name);
        }
        self.save()
    }

    /// Update a server's `url` and/or bearer token in place. `name` is immutable (it is the
    /// `mcp__<name>__<tool>` routing key spliced into model-facing tool names — renaming would
    /// silently break any in-flight context referencing the old prefix); callers wanting a new
    /// name must `remove` + `add` instead. Validates `url` exactly like [`Self::add`] and errs on
    /// an unknown name with the same message shape [`Self::set_enabled`]/[`Self::remove`] use, so
    /// the web handler can tell "bad input" (400) from "stale panel" (404) apart by re-checking
    /// existence itself before calling this (see `web::routes::mcp::update_server`). Persists on
    /// success. Deliberately leaves the cached `tools_bytes` entry untouched — an edit to the same
    /// server's url/token is very likely still the same tool set, and a stale-but-honest cached
    /// size beats silently zeroing the usage meter until the next probe.
    pub fn update(&self, name: &str, url: String, token_change: TokenChange) -> Result<(), String> {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(format!(
                "invalid server url '{url}': must start with http:// or https://"
            ));
        }
        {
            let mut servers = self.write_lock();
            let Some(server) = servers.iter_mut().find(|s| s.name == name) else {
                return Err(format!("no mcp server named '{name}'"));
            };
            server.url = url;
            match token_change {
                TokenChange::Keep => {}
                TokenChange::Clear => server.bearer_token = None,
                TokenChange::Set(token) => server.bearer_token = Some(token),
            }
        }
        self.save()
    }

    /// Every configured server, enabled or not (the Settings page shows the full list).
    pub fn list(&self) -> Vec<McpServerConfig> {
        self.read_lock().clone()
    }

    /// Only the enabled servers — the accessor the LLM backends consult per turn.
    pub fn enabled(&self) -> Vec<McpServerConfig> {
        self.read_lock()
            .iter()
            .filter(|s| s.enabled)
            .cloned()
            .collect()
    }

    /// Mirror the in-memory list to disk via a same-directory tmp+rename (same crash-safety
    /// rationale as `vault::store::write_atomic`, implemented locally to keep `mcp` free of a
    /// `vault` dependency). Called after every successful mutation, outside the servers lock —
    /// `save` retakes a read lock itself, and holding the write lock across file I/O would stall
    /// every concurrent `enabled()` snapshot on a slow disk.
    fn save(&self) -> Result<(), String> {
        let rendered = serde_json::to_string_pretty(&*self.read_lock())
            .map_err(|e| format!("serializing mcp server config: {e}"))?;
        write_atomic(&self.path, &rendered)
            .map_err(|e| format!("writing {}: {e}", self.path.display()))
    }

    fn read_lock(&self) -> std::sync::RwLockReadGuard<'_, Vec<McpServerConfig>> {
        self.servers.read().expect("mcp registry lock poisoned")
    }

    fn write_lock(&self) -> std::sync::RwLockWriteGuard<'_, Vec<McpServerConfig>> {
        self.servers.write().expect("mcp registry lock poisoned")
    }
}

/// Write `contents` to `path` via a unique sibling `*.tmp-*` file + rename — atomic on the same
/// filesystem, and the unique suffix keeps concurrent writers from consuming each other's temp
/// file (mirrors `vault::store::write_atomic`, see the module doc for why it is not shared).
fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp-{}-{}", std::process::id(), n));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, contents)?;
    // The file carries bearer tokens (a secret at rest, ADR-0018): owner-only before the rename
    // publishes it — `rename` preserves the tmp file's mode, so the visible file is 0600 too.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(name: &str, enabled: bool) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            url: format!("http://mcp.example/{name}"),
            bearer_token: None,
            enabled,
        }
    }

    #[test]
    fn missing_file_is_an_empty_registry() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = McpRegistry::load(tmp.path().join(".mcp-servers.json"));
        assert!(reg.list().is_empty());
        assert!(reg.enabled().is_empty());
    }

    #[test]
    fn unparsable_file_degrades_to_empty_not_a_crash() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".mcp-servers.json");
        std::fs::write(&path, "{ this is not json").unwrap();
        let reg = McpRegistry::load(&path);
        assert!(reg.list().is_empty());
        // The broken file survives until the first mutation overwrites it.
        assert!(std::fs::read_to_string(&path).unwrap().contains("not json"));
    }

    #[test]
    fn add_toggle_remove_round_trips_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".mcp-servers.json");

        let reg = McpRegistry::load(&path);
        reg.add(McpServerConfig {
            name: "tracker".to_string(),
            url: "https://mcp.example/rpc".to_string(),
            bearer_token: Some("tok".to_string()),
            enabled: true,
        })
        .unwrap();
        reg.add(server("files", false)).unwrap();

        // A fresh load sees exactly what was persisted, token included.
        let reloaded = McpRegistry::load(&path);
        assert_eq!(reloaded.list().len(), 2);
        assert_eq!(reloaded.list()[0].bearer_token.as_deref(), Some("tok"));
        assert_eq!(
            reloaded
                .enabled()
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["tracker"],
            "enabled() filters the disabled server"
        );

        // Toggle persists…
        reloaded.set_enabled("files", true).unwrap();
        assert_eq!(McpRegistry::load(&path).enabled().len(), 2);
        // …and so does removal.
        reloaded.remove("tracker").unwrap();
        let final_load = McpRegistry::load(&path);
        assert_eq!(
            final_load
                .list()
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["files"]
        );
    }

    #[test]
    fn add_rejects_invalid_names_urls_and_duplicates() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = McpRegistry::load(tmp.path().join(".mcp-servers.json"));

        // Names outside the [a-z0-9-] slug alphabet (the tool-name prefix contract).
        for bad in ["", "Has Caps", "under_score", "dots.too", "mcp__x"] {
            let mut cfg = server("ok", true);
            cfg.name = bad.to_string();
            assert!(reg.add(cfg).is_err(), "name '{bad}' must be rejected");
        }
        // Non-http(s) URLs.
        for bad in ["ftp://x", "file:///etc/passwd", "mcp.example/rpc", ""] {
            let mut cfg = server("ok", true);
            cfg.url = bad.to_string();
            assert!(reg.add(cfg).is_err(), "url '{bad}' must be rejected");
        }
        // Duplicate name.
        reg.add(server("dupe", true)).unwrap();
        let err = reg.add(server("dupe", false)).unwrap_err();
        assert!(err.contains("already exists"));

        // Nothing invalid leaked into the list.
        assert_eq!(reg.list().len(), 1);
    }

    #[test]
    fn toggle_and_remove_unknown_names_are_readable_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = McpRegistry::load(tmp.path().join(".mcp-servers.json"));
        assert!(reg
            .set_enabled("ghost", true)
            .unwrap_err()
            .contains("ghost"));
        assert!(reg.remove("ghost").unwrap_err().contains("ghost"));
    }

    #[test]
    fn update_can_change_url_and_apply_each_token_change_variant() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".mcp-servers.json");
        let reg = McpRegistry::load(&path);
        reg.add(McpServerConfig {
            name: "tracker".to_string(),
            url: "https://mcp.example/rpc".to_string(),
            bearer_token: Some("orig-token".to_string()),
            enabled: true,
        })
        .unwrap();

        // Keep: url changes, token untouched.
        reg.update(
            "tracker",
            "https://mcp.example/v2".to_string(),
            TokenChange::Keep,
        )
        .unwrap();
        let reloaded = McpRegistry::load(&path);
        assert_eq!(reloaded.list()[0].url, "https://mcp.example/v2");
        assert_eq!(
            reloaded.list()[0].bearer_token.as_deref(),
            Some("orig-token"),
            "Keep must not touch the stored token"
        );

        // Set: replaces the token.
        reg.update(
            "tracker",
            "https://mcp.example/v2".to_string(),
            TokenChange::Set("new-token".to_string()),
        )
        .unwrap();
        assert_eq!(
            McpRegistry::load(&path).list()[0].bearer_token.as_deref(),
            Some("new-token")
        );

        // Clear: erases the token even though a url is also supplied.
        reg.update(
            "tracker",
            "https://mcp.example/v2".to_string(),
            TokenChange::Clear,
        )
        .unwrap();
        assert_eq!(McpRegistry::load(&path).list()[0].bearer_token, None);
    }

    #[test]
    fn update_rejects_invalid_url_and_unknown_name() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = McpRegistry::load(tmp.path().join(".mcp-servers.json"));
        reg.add(server("tracker", true)).unwrap();

        let err = reg
            .update("tracker", "ftp://bad".to_string(), TokenChange::Keep)
            .unwrap_err();
        assert!(err.contains("invalid server url"));

        let err = reg
            .update(
                "ghost",
                "https://mcp.example/rpc".to_string(),
                TokenChange::Keep,
            )
            .unwrap_err();
        assert!(err.contains("ghost"));

        // Neither failure mutated the one real server.
        assert_eq!(reg.list()[0].url, "http://mcp.example/tracker");
    }

    #[test]
    fn name_validation_alphabet() {
        assert!(is_valid_name("a"));
        assert!(is_valid_name("my-tracker-2"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("A"));
        assert!(!is_valid_name("a b"));
        assert!(!is_valid_name("a_b"));
    }
    #[test]
    fn tools_bytes_cache_sums_enabled_only_and_clears_on_remove() {
        let dir = std::env::temp_dir().join(format!("mcp-cache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let reg = McpRegistry::load(dir.join("cfg.json"));
        for (name, enabled) in [("on", true), ("off", false)] {
            reg.add(McpServerConfig {
                name: name.into(),
                url: "http://x/mcp".into(),
                bearer_token: None,
                enabled,
            })
            .unwrap();
        }
        reg.note_tools_bytes("on", 7_000);
        reg.note_tools_bytes("off", 5_000);
        assert_eq!(
            reg.enabled_tools_bytes(),
            7_000,
            "disabled servers don't count"
        );
        reg.set_enabled("off", true).unwrap();
        assert_eq!(reg.enabled_tools_bytes(), 12_000);
        reg.remove("on").unwrap();
        assert_eq!(
            reg.enabled_tools_bytes(),
            5_000,
            "removed server's cache entry is gone"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
