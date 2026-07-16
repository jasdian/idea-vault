//! Persistent named-source registry — the owner's list of reference sources (a `name` → absolute
//! `host_path` pair each), stored as one JSON file (`<vault>/.sources.json`) plus a *generated*
//! compose override (`<vault>/.docker-compose.sources.yml`) that bind-mounts each source
//! read-only at `/mnt/sources/<name>`.
//!
//! **This is app config, NOT vault truth.** The dotfiles live beside the ideas only because the
//! vault bind mount is the one host-persistent path in a containerized run — they must never
//! enter the SQLite index (`vault::walk::walk_ideas` only admits *directories* containing an
//! `idea.md`, so top-level dotfiles are invisible to reindex by construction) and losing them
//! costs the owner a re-add of source paths, not ideas.
//!
//! **DRT (deterministic tool leaves):** the model never sees or chooses filesystem paths. It
//! picks *which* source (by name) and *what* to look for; code resolves the name to a root via
//! this registry ([`SourceRegistry::resolve_attached`]) and enforces containment. Names share
//! the slug alphabet (`domain::slug::is_valid`) because they double as the mount target and the
//! tool routing key.
//!
//! **The app never runs docker (ADR-0020).** Mutations here rewrite the override file; the owner
//! applies it with `docker compose up -d`. The gap between "saved" and "applied" is surfaced as
//! [`SourceStatus::NeedsReup`], computed by comparing the registry's [`SourceRegistry::fingerprint`]
//! against the `IDEA_VAULT_SOURCES_APPLIED` value the override baked into the container env at
//! `up` time.
//!
//! Failure discipline matches the rest of the crate: a missing file is an empty registry, an
//! unparsable file is a warning + empty registry (boot must never crash on owner-editable JSON),
//! and every mutation persists via a same-directory tmp+rename so a crash mid-save can never
//! leave a half-written file. The atomic-write helper is implemented locally rather than reused
//! from `vault::store` — depending on `vault` here would put app config inside the truth module.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

use crate::domain::slug;

/// Well-known basename of the generated compose override, beside the vault's `.sources.json`.
/// The owner layers it with `docker compose -f docker-compose.yml -f vault/.docker-compose.sources.yml up -d`
/// (or a `COMPOSE_FILE` entry) — the app itself never invokes docker (ADR-0020).
pub const OVERRIDE_FILENAME: &str = ".docker-compose.sources.yml";

/// One registered reference source: the owner's chosen `name` and the absolute host directory it
/// points at. `name` is restricted to the slug alphabet `[a-z0-9-]` because it is used verbatim
/// as the container mount target (`/mnt/sources/<name>`) and as the tool routing key the model
/// picks sources by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceConfig {
    pub name: String,
    pub host_path: PathBuf,
}

/// A source resolved for an AI turn. `root` is **canonical** (`std::fs::canonicalize` has already
/// succeeded on it) — that invariant is the tested precondition of the ai-side symlink-escape
/// gate, which checks that every path a tool leaf touches stays under a canonical root. A
/// `ResolvedSource` with a non-canonical root must never be constructed; get them from
/// [`SourceRegistry::resolve_attached`] only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSource {
    pub name: String,
    /// Canonical root directory of the source (see the struct doc — this is an invariant, not a
    /// convention).
    pub root: PathBuf,
}

/// Health of one registered source, as shown on the Sources page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceStatus {
    /// The resolved directory is listable. `entries` is the direct child count — `entries == 0`
    /// is the ADR-0020 ghost-bind signal (a bind mount whose host side vanished lists as an
    /// empty directory instead of erroring), so the UI renders `Mounted { entries: 0 }` as a
    /// warning, not a green light.
    Mounted { entries: usize },
    /// Container mode only: the registry entry differs from what the running container had
    /// applied at `up` time (added or edited since the last `docker compose up`, or the override
    /// was never layered at all). The owner must re-`up` for the bind mount to exist.
    NeedsReup,
    /// The resolved directory is not listable — in bare mode the host path is gone/unreadable;
    /// in container mode the bind itself is broken.
    Missing,
}

/// The registry: an in-memory source list mirrored to `config_path` after every mutation, with
/// the compose override regenerated in lock-step. `Arc`'d into `AppState` so a Sources-page edit
/// is visible to the very next model turn with no restart — same live-tuning discipline as
/// `LlmSettings`.
pub struct SourceRegistry {
    /// The JSON source list, e.g. `<vault>/.sources.json`.
    config_path: PathBuf,
    /// The generated compose override, `<vault>/`[`OVERRIDE_FILENAME`].
    override_path: PathBuf,
    /// `IDEA_VAULT_SOURCES_DIR`: `Some(/mnt/sources)` in-container, `None` in a bare
    /// `cargo run` (host paths are then read directly).
    mount_root: Option<PathBuf>,
    /// `IDEA_VAULT_SOURCES_APPLIED` as seen at boot: the fingerprint the override baked into the
    /// container env at `up` time. `None` means a bare run or an override that was never
    /// layered — in container mode that reads as "NO source is applied".
    applied_fingerprint: Option<String>,
    sources: RwLock<Vec<SourceConfig>>,
}

impl SourceRegistry {
    /// Load the registry from `config_path` at boot. Missing file ⇒ empty list;
    /// unreadable/unparsable file ⇒ `tracing::warn` + empty list — a corrupt config file must
    /// never crash boot (the owner re-adds sources on the Sources page; the broken file is only
    /// overwritten on the next mutation, so it stays inspectable until then). Also regenerates
    /// the compose override once (so it is a pure function of the registry even if a previous
    /// run crashed between save and regen — a failed regen at boot is a warning, never a crash)
    /// and ensures the vault `.gitignore` covers both dotfiles.
    pub fn load(
        config_path: impl Into<PathBuf>,
        override_path: impl Into<PathBuf>,
        mount_root: Option<PathBuf>,
        applied_fingerprint: Option<String>,
    ) -> Self {
        let config_path = config_path.into();
        let sources = match std::fs::read_to_string(&config_path) {
            Ok(raw) => match serde_json::from_str::<Vec<SourceConfig>>(&raw) {
                Ok(list) => list,
                Err(e) => {
                    tracing::warn!(
                        path = %config_path.display(),
                        error = %e,
                        "source registry unparsable; starting with an empty registry"
                    );
                    Vec::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                tracing::warn!(
                    path = %config_path.display(),
                    error = %e,
                    "source registry unreadable; starting with an empty registry"
                );
                Vec::new()
            }
        };
        let registry = Self {
            config_path,
            override_path: override_path.into(),
            mount_root,
            applied_fingerprint,
            sources: RwLock::new(sources),
        };
        if let Err(e) = registry.regenerate_override() {
            tracing::warn!(
                error = %e,
                "could not regenerate the sources compose override at boot; \
                 the next mutation retries"
            );
        }
        registry.ensure_vault_gitignore();
        registry
    }

    /// Add a source: reject an invalid name, an unsafe host path, or a duplicate name *or* host
    /// path (two names for one directory would silently alias the same content under two tool
    /// routing keys). Persists and regenerates the override on success; if the regen fails after
    /// a successful save the registry state is KEPT and the error returned — the next mutation
    /// or boot retries the regen.
    pub fn add(&self, cfg: SourceConfig) -> Result<(), String> {
        if !slug::is_valid(&cfg.name) {
            return Err(format!(
                "invalid source name '{}': use lowercase letters, digits and '-' only",
                cfg.name
            ));
        }
        validate_host_path(&cfg.host_path)?;
        {
            let mut sources = self.write_lock();
            if sources.iter().any(|s| s.name == cfg.name) {
                return Err(format!("a source named '{}' already exists", cfg.name));
            }
            if let Some(existing) = sources.iter().find(|s| s.host_path == cfg.host_path) {
                return Err(format!(
                    "host path '{}' is already registered as source '{}'",
                    cfg.host_path.display(),
                    existing.name
                ));
            }
            sources.push(cfg);
        }
        self.save()?;
        self.regenerate_override()
    }

    /// Update a source's `host_path` in place. `name` is immutable — it is the container mount
    /// target (`/mnt/sources/<name>`) and the tool routing key, so renaming would silently break
    /// idea frontmatter `sources:` lists and any in-flight context referencing the old name;
    /// callers wanting a new name must `remove` + `add` instead. Validates the path exactly like
    /// [`Self::add`] and errs on an unknown name. Same persist/regen contract as [`Self::add`].
    pub fn update_path(&self, name: &str, host_path: PathBuf) -> Result<(), String> {
        validate_host_path(&host_path)?;
        {
            let mut sources = self.write_lock();
            if let Some(existing) = sources
                .iter()
                .find(|s| s.name != name && s.host_path == host_path)
            {
                return Err(format!(
                    "host path '{}' is already registered as source '{}'",
                    host_path.display(),
                    existing.name
                ));
            }
            let Some(source) = sources.iter_mut().find(|s| s.name == name) else {
                return Err(format!("no source named '{name}'"));
            };
            source.host_path = host_path;
        }
        self.save()?;
        self.regenerate_override()
    }

    /// Remove one source. Errs on an unknown name so a stale Sources form gets a readable
    /// failure instead of silently doing nothing. Same persist/regen contract as [`Self::add`].
    pub fn remove(&self, name: &str) -> Result<(), String> {
        {
            let mut sources = self.write_lock();
            let before = sources.len();
            sources.retain(|s| s.name != name);
            if sources.len() == before {
                return Err(format!("no source named '{name}'"));
            }
        }
        self.save()?;
        self.regenerate_override()
    }

    /// Every configured source (the Sources page shows the full list).
    pub fn list(&self) -> Vec<SourceConfig> {
        self.read_lock().clone()
    }

    /// One source by name.
    pub fn get(&self, name: &str) -> Option<SourceConfig> {
        self.read_lock().iter().find(|s| s.name == name).cloned()
    }

    /// Where a source's content lives *from this process's point of view*: the container mount
    /// point (`<mount_root>/<name>`) when running in-container, the raw host path in a bare run.
    /// NOT canonicalized and NOT a safety boundary — this is for status probes and UI display
    /// only. Anything handing a root to model-driven tool leaves must go through
    /// [`Self::resolve_attached`], which canonicalizes.
    pub fn resolve_one(&self, name: &str) -> Option<PathBuf> {
        let cfg = self.get(name)?;
        Some(match &self.mount_root {
            Some(root) => root.join(&cfg.name),
            None => cfg.host_path,
        })
    }

    /// Resolve an idea's attached source names (its frontmatter `sources:` list) into canonical
    /// roots for an AI turn. Per name: [`Self::resolve_one`], then `std::fs::canonicalize` —
    /// unknown names and roots that fail to canonicalize (not yet mounted, deleted, permission
    /// denied) are DROPPED with a `tracing::warn`, never an error: a stale attachment must
    /// degrade the turn, not kill it. The canonical root in every returned [`ResolvedSource`] is
    /// the tested precondition of the ai-side symlink-escape gate — see [`ResolvedSource::root`].
    pub fn resolve_attached(&self, names: &[String]) -> Vec<ResolvedSource> {
        names
            .iter()
            .filter_map(|name| {
                let Some(path) = self.resolve_one(name) else {
                    tracing::warn!(
                        source = %name,
                        "attached source is not in the registry; dropping it from this turn"
                    );
                    return None;
                };
                match std::fs::canonicalize(&path) {
                    Ok(root) => Some(ResolvedSource {
                        name: name.clone(),
                        root,
                    }),
                    Err(e) => {
                        tracing::warn!(
                            source = %name,
                            path = %path.display(),
                            error = %e,
                            "attached source root not canonicalizable; dropping it from this turn"
                        );
                        None
                    }
                }
            })
            .collect()
    }

    /// The registry's identity for "is the running container up to date": sources sorted by
    /// name, each rendered `name=host_path`, joined with `;`. Empty registry ⇒ `""`.
    /// Deliberately human-debuggable (no hashing) — `docker inspect` on the container env shows
    /// exactly which name→path pairs were applied.
    pub fn fingerprint(&self) -> String {
        fingerprint_of(&self.sorted_snapshot())
    }

    /// Health of one source ([`None`] for an unknown name). Bare mode: the host path is listable
    /// or it is [`SourceStatus::Missing`]. Container mode: the entry must first appear verbatim
    /// in the applied fingerprint (else [`SourceStatus::NeedsReup`] — covers "added/edited since
    /// the last `up`" and "override never layered"), and only then is the mount point probed.
    pub fn status(&self, name: &str) -> Option<SourceStatus> {
        let cfg = self.get(name)?;
        Some(self.status_of(&cfg))
    }

    /// Every source with its health, in registry order — the Sources page's row model.
    pub fn statuses(&self) -> Vec<(SourceConfig, SourceStatus)> {
        self.list()
            .into_iter()
            .map(|cfg| {
                let status = self.status_of(&cfg);
                (cfg, status)
            })
            .collect()
    }

    fn status_of(&self, cfg: &SourceConfig) -> SourceStatus {
        let probe_path = match &self.mount_root {
            None => cfg.host_path.clone(),
            Some(root) => {
                // A host path containing ';' would split wrong here — that can only bias toward
                // NeedsReup, the safe direction (owner re-ups; nothing is wrongly trusted).
                let pair = format!("{}={}", cfg.name, cfg.host_path.display());
                let applied: HashSet<&str> = self
                    .applied_fingerprint
                    .as_deref()
                    .map(|f| f.split(';').filter(|p| !p.is_empty()).collect())
                    .unwrap_or_default();
                if !applied.contains(pair.as_str()) {
                    return SourceStatus::NeedsReup;
                }
                root.join(&cfg.name)
            }
        };
        match std::fs::read_dir(&probe_path) {
            Ok(entries) => SourceStatus::Mounted {
                entries: entries.count(),
            },
            Err(_) => SourceStatus::Missing,
        }
    }

    /// Rewrite the compose override as a pure function of the registry (idempotent — same
    /// registry, same bytes). Zero sources still renders the file (with an empty applied
    /// fingerprint and no `volumes:` key) so a `COMPOSE_FILE` that lists the override keeps
    /// working. Atomic like every other write here.
    pub fn regenerate_override(&self) -> Result<(), String> {
        write_atomic(&self.override_path, &self.render_override())
            .map_err(|e| format!("writing {}: {e}", self.override_path.display()))
    }

    fn render_override(&self) -> String {
        let sources = self.sorted_snapshot();
        let mut out = String::new();
        out.push_str(
            "# GENERATED by idea-vault — do not edit. Managed from the Sources page (/sources).\n",
        );
        out.push_str(
            "# Apply changes: docker compose up -d   (the app never runs docker — ADR-0020)\n",
        );
        out.push_str("services:\n  idea-vault:\n    environment:\n");
        out.push_str(&format!(
            "      IDEA_VAULT_SOURCES_APPLIED: {}\n",
            yaml_scalar(&fingerprint_of(&sources))
        ));
        if !sources.is_empty() {
            out.push_str("    volumes:\n");
            for s in &sources {
                out.push_str("      - type: bind\n");
                out.push_str(&format!(
                    "        source: {}\n",
                    yaml_scalar(&s.host_path.display().to_string())
                ));
                // Names are [a-z0-9-] by construction — no escaping needed in the target.
                out.push_str(&format!("        target: /mnt/sources/{}\n", s.name));
                out.push_str("        read_only: true\n");
                out.push_str("        bind:\n          create_host_path: false\n");
            }
        }
        out
    }

    /// Make sure the vault dir (config_path's parent) has a `.gitignore` covering both dotfiles —
    /// create it if absent, append only the missing lines, NEVER rewrite or remove owner content.
    /// Rationale: CLAUDE.md invites a nested owner git repo *inside* `vault/`, which the app
    /// repo's own `.gitignore` cannot protect, and host paths are machine-identifying — they must
    /// not leak into an ideas repo the owner might publish. Best-effort: failures are warnings
    /// (the registry still works; the owner just loses the ignore convenience).
    fn ensure_vault_gitignore(&self) {
        let Some(dir) = self.config_path.parent() else {
            return;
        };
        let gitignore = dir.join(".gitignore");
        let wanted: Vec<&str> = [&self.config_path, &self.override_path]
            .into_iter()
            .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
            .collect();
        let existing = match std::fs::read_to_string(&gitignore) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                tracing::warn!(
                    path = %gitignore.display(),
                    error = %e,
                    "could not read the vault .gitignore; leaving it alone"
                );
                return;
            }
        };
        let have: HashSet<&str> = existing.lines().map(str::trim).collect();
        let missing: Vec<&&str> = wanted.iter().filter(|w| !have.contains(**w)).collect();
        if missing.is_empty() {
            return;
        }
        let mut appended = String::new();
        if !existing.is_empty() && !existing.ends_with('\n') {
            appended.push('\n');
        }
        for line in missing {
            appended.push_str(line);
            appended.push('\n');
        }
        let result = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&gitignore)
            .and_then(|mut f| std::io::Write::write_all(&mut f, appended.as_bytes()));
        if let Err(e) = result {
            tracing::warn!(
                path = %gitignore.display(),
                error = %e,
                "could not extend the vault .gitignore"
            );
        }
    }

    /// Mirror the in-memory list to disk via a same-directory tmp+rename (same crash-safety
    /// rationale as `vault::store::write_atomic`, implemented locally to keep `sources` free of
    /// a `vault` dependency). Called after every successful mutation, outside the sources lock —
    /// `save` retakes a read lock itself, and holding the write lock across file I/O would stall
    /// every concurrent `resolve_attached` on a slow disk.
    fn save(&self) -> Result<(), String> {
        let rendered = serde_json::to_string_pretty(&*self.read_lock())
            .map_err(|e| format!("serializing source registry: {e}"))?;
        write_atomic(&self.config_path, &rendered)
            .map_err(|e| format!("writing {}: {e}", self.config_path.display()))?;
        self.ensure_vault_gitignore();
        Ok(())
    }

    /// A clone of the list sorted by name — the deterministic order both the fingerprint and the
    /// override render in.
    fn sorted_snapshot(&self) -> Vec<SourceConfig> {
        let mut sources = self.read_lock().clone();
        sources.sort_by(|a, b| a.name.cmp(&b.name));
        sources
    }

    fn read_lock(&self) -> std::sync::RwLockReadGuard<'_, Vec<SourceConfig>> {
        self.sources.read().expect("source registry lock poisoned")
    }

    fn write_lock(&self) -> std::sync::RwLockWriteGuard<'_, Vec<SourceConfig>> {
        self.sources.write().expect("source registry lock poisoned")
    }
}

/// `name=host_path` pairs joined with `;` over an already-name-sorted slice — shared by
/// [`SourceRegistry::fingerprint`] and the override render so the two can never disagree.
fn fingerprint_of(sources: &[SourceConfig]) -> String {
    sources
        .iter()
        .map(|s| format!("{}={}", s.name, s.host_path.display()))
        .collect::<Vec<_>>()
        .join(";")
}

/// Reject host paths that could escape, alias, or corrupt the generated compose file. Checked at
/// registration so the override render can trust every stored path: absolute, not the filesystem
/// root (mounting `/` read-only into the model's reach is never what the owner meant), no `..`
/// component, and no newline/CR/NUL anywhere (a newline inside a YAML scalar is the classic
/// compose-injection vector; serde escaping would survive it, but there is no legitimate reason
/// to allow it at all).
fn validate_host_path(path: &Path) -> Result<(), String> {
    let raw = path.as_os_str().as_encoded_bytes();
    if raw.iter().any(|b| matches!(b, b'\n' | b'\r' | 0)) {
        return Err(format!(
            "invalid host path {path:?}: newlines, carriage returns and NUL bytes are not allowed"
        ));
    }
    if !path.is_absolute() {
        return Err(format!(
            "invalid host path '{}': must be an absolute path",
            path.display()
        ));
    }
    if path.parent().is_none() {
        return Err(format!(
            "invalid host path '{}': the filesystem root cannot be a source",
            path.display()
        ));
    }
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(format!(
            "invalid host path '{}': '..' components are not allowed",
            path.display()
        ));
    }
    Ok(())
}

/// Render one string as a YAML scalar exactly as compose will parse it back — serde_norway quotes
/// precisely when YAML requires it (empty string, leading specials, `: `, …) and leaves plain
/// scalars bare. The serializer's trailing newline is trimmed because the caller splices the
/// scalar into a hand-indented line.
fn yaml_scalar(s: &str) -> String {
    serde_norway::to_string(s)
        .expect("serializing a string to a yaml scalar cannot fail")
        .trim_end_matches('\n')
        .to_string()
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
    // Host paths are machine-identifying (which boxes the owner has, and where their notes
    // live): owner-only before the rename publishes it — `rename` preserves the tmp file's
    // mode, so the visible file is 0600 too.
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

    /// A registry in a fresh tempdir, bare mode. Returns the tempdir too — dropping it deletes
    /// the files.
    fn bare_registry(dir: &tempfile::TempDir) -> SourceRegistry {
        SourceRegistry::load(
            dir.path().join(".sources.json"),
            dir.path().join(OVERRIDE_FILENAME),
            None,
            None,
        )
    }

    fn source(name: &str, host_path: &str) -> SourceConfig {
        SourceConfig {
            name: name.to_string(),
            host_path: PathBuf::from(host_path),
        }
    }

    #[test]
    fn missing_file_is_an_empty_registry() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = bare_registry(&tmp);
        assert!(reg.list().is_empty());
        assert_eq!(reg.fingerprint(), "");
    }

    #[test]
    fn unparsable_file_degrades_to_empty_not_a_crash() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".sources.json");
        std::fs::write(&path, "{ this is not json").unwrap();
        let reg = SourceRegistry::load(&path, tmp.path().join(OVERRIDE_FILENAME), None, None);
        assert!(reg.list().is_empty());
        // The broken file survives until the first mutation overwrites it.
        assert!(std::fs::read_to_string(&path).unwrap().contains("not json"));
    }

    #[test]
    fn add_update_remove_round_trip_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join(".sources.json");
        let overr = tmp.path().join(OVERRIDE_FILENAME);

        let reg = SourceRegistry::load(&config, &overr, None, None);
        reg.add(source("docs", "/home/owner/docs")).unwrap();
        reg.add(source("notes", "/home/owner/notes")).unwrap();

        // A fresh load sees exactly what was persisted.
        let reloaded = SourceRegistry::load(&config, &overr, None, None);
        assert_eq!(reloaded.list().len(), 2);
        assert_eq!(
            reloaded.get("docs"),
            Some(source("docs", "/home/owner/docs"))
        );

        // update_path persists…
        reloaded
            .update_path("docs", PathBuf::from("/srv/docs"))
            .unwrap();
        assert_eq!(
            SourceRegistry::load(&config, &overr, None, None)
                .get("docs")
                .unwrap()
                .host_path,
            PathBuf::from("/srv/docs")
        );
        // …and so does removal.
        reloaded.remove("notes").unwrap();
        let final_load = SourceRegistry::load(&config, &overr, None, None);
        assert_eq!(
            final_load
                .list()
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["docs"]
        );
    }

    #[test]
    fn validation_rejects_bad_names_paths_and_duplicates() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = bare_registry(&tmp);

        // Names outside the [a-z0-9-] slug alphabet (mount-target + routing-key contract).
        for bad in ["", "Has Caps", "under_score", "dots.too", "a/b"] {
            let err = reg.add(source(bad, "/home/owner/docs")).unwrap_err();
            assert!(err.contains("invalid source name"), "name '{bad}': {err}");
        }
        // Relative path.
        let err = reg.add(source("docs", "relative/docs")).unwrap_err();
        assert!(err.contains("absolute"), "{err}");
        // Filesystem root.
        let err = reg.add(source("docs", "/")).unwrap_err();
        assert!(err.contains("filesystem root"), "{err}");
        // '..' component.
        let err = reg.add(source("docs", "/home/owner/../etc")).unwrap_err();
        assert!(err.contains(".."), "{err}");
        // Newline / CR / NUL (compose-injection guard).
        for bad in [
            "/home/owner\ndocs",
            "/home/owner\rdocs",
            "/home/owner\0docs",
        ] {
            let err = reg.add(source("docs", bad)).unwrap_err();
            assert!(err.contains("not allowed"), "path {bad:?}: {err}");
        }

        reg.add(source("docs", "/home/owner/docs")).unwrap();
        // Duplicate name.
        let err = reg.add(source("docs", "/somewhere/else")).unwrap_err();
        assert!(err.contains("already exists"), "{err}");
        // Duplicate host path (silent aliasing) — the error names the existing entry.
        let err = reg.add(source("docs2", "/home/owner/docs")).unwrap_err();
        assert!(err.contains("'docs'"), "{err}");
        // update_path enforces the same aliasing guard and the unknown-name error.
        reg.add(source("notes", "/home/owner/notes")).unwrap();
        let err = reg
            .update_path("notes", PathBuf::from("/home/owner/docs"))
            .unwrap_err();
        assert!(err.contains("'docs'"), "{err}");
        assert!(reg
            .update_path("ghost", PathBuf::from("/home/owner/x"))
            .unwrap_err()
            .contains("ghost"));
        assert!(reg.remove("ghost").unwrap_err().contains("ghost"));

        // Nothing invalid leaked into the list.
        assert_eq!(reg.list().len(), 2);
    }

    #[test]
    fn fingerprint_is_sorted_by_name_regardless_of_insertion_order() {
        let tmp_a = tempfile::tempdir().unwrap();
        let tmp_b = tempfile::tempdir().unwrap();
        let reg_a = bare_registry(&tmp_a);
        let reg_b = bare_registry(&tmp_b);

        reg_a.add(source("alpha", "/srv/alpha")).unwrap();
        reg_a.add(source("beta", "/srv/beta")).unwrap();
        reg_b.add(source("beta", "/srv/beta")).unwrap();
        reg_b.add(source("alpha", "/srv/alpha")).unwrap();

        assert_eq!(reg_a.fingerprint(), reg_b.fingerprint());
        assert_eq!(reg_a.fingerprint(), "alpha=/srv/alpha;beta=/srv/beta");
    }

    #[test]
    fn resolve_one_uses_host_path_bare_and_mount_root_in_container() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = bare_registry(&tmp);
        bare.add(source("docs", "/home/owner/docs")).unwrap();
        assert_eq!(
            bare.resolve_one("docs"),
            Some(PathBuf::from("/home/owner/docs"))
        );
        assert_eq!(bare.resolve_one("ghost"), None);

        let tmp2 = tempfile::tempdir().unwrap();
        let container = SourceRegistry::load(
            tmp2.path().join(".sources.json"),
            tmp2.path().join(OVERRIDE_FILENAME),
            Some(PathBuf::from("/mnt/sources")),
            None,
        );
        container.add(source("docs", "/home/owner/docs")).unwrap();
        assert_eq!(
            container.resolve_one("docs"),
            Some(PathBuf::from("/mnt/sources/docs"))
        );
    }

    #[test]
    fn resolve_attached_drops_unknown_and_missing_and_returns_canonical_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tempfile::tempdir().unwrap();
        let reg = bare_registry(&tmp);
        reg.add(source("real", real.path().to_str().unwrap()))
            .unwrap();
        reg.add(source("gone", "/nonexistent/idea-vault-test-path"))
            .unwrap();

        let resolved = reg.resolve_attached(&[
            "real".to_string(),
            "gone".to_string(),
            "unknown".to_string(),
        ]);
        assert_eq!(
            resolved,
            vec![ResolvedSource {
                name: "real".to_string(),
                // Canonical, not merely the registered path — /tmp itself may be a symlink.
                root: std::fs::canonicalize(real.path()).unwrap(),
            }]
        );
    }

    #[test]
    fn status_bare_mode_probes_the_host_path() {
        let tmp = tempfile::tempdir().unwrap();
        let host = tempfile::tempdir().unwrap();
        std::fs::write(host.path().join("a.md"), "x").unwrap();
        std::fs::create_dir(host.path().join("sub")).unwrap();

        let reg = bare_registry(&tmp);
        reg.add(source("docs", host.path().to_str().unwrap()))
            .unwrap();
        reg.add(source("gone", "/nonexistent/idea-vault-test-path"))
            .unwrap();

        assert_eq!(
            reg.status("docs"),
            Some(SourceStatus::Mounted { entries: 2 })
        );
        assert_eq!(reg.status("gone"), Some(SourceStatus::Missing));
        assert_eq!(reg.status("unknown"), None);
        assert_eq!(reg.statuses().len(), 2);
    }

    #[test]
    fn status_container_mode_matrix() {
        let tmp = tempfile::tempdir().unwrap();
        let mount = tempfile::tempdir().unwrap(); // stands in for /mnt/sources
        let config = tmp.path().join(".sources.json");
        let overr = tmp.path().join(OVERRIDE_FILENAME);

        // Applied fingerprint covers docs (matching pair) and stale (an OLD path).
        let applied = "docs=/home/owner/docs;stale=/old/path".to_string();
        let reg = SourceRegistry::load(
            &config,
            &overr,
            Some(mount.path().to_path_buf()),
            Some(applied),
        );
        reg.add(source("docs", "/home/owner/docs")).unwrap();
        reg.add(source("stale", "/new/path")).unwrap();
        reg.add(source("fresh", "/home/owner/fresh")).unwrap();

        // Applied matches but the mount dir is gone => the bind is broken.
        assert_eq!(reg.status("docs"), Some(SourceStatus::Missing));
        // Applied matches + empty dir listable => Mounted{0}, the ghost-bind warning signal.
        std::fs::create_dir(mount.path().join("docs")).unwrap();
        assert_eq!(
            reg.status("docs"),
            Some(SourceStatus::Mounted { entries: 0 })
        );
        std::fs::write(mount.path().join("docs/a.md"), "x").unwrap();
        assert_eq!(
            reg.status("docs"),
            Some(SourceStatus::Mounted { entries: 1 })
        );
        // Pair mismatch (path edited since the last up) => NeedsReup even if a mount dir exists.
        std::fs::create_dir(mount.path().join("stale")).unwrap();
        assert_eq!(reg.status("stale"), Some(SourceStatus::NeedsReup));
        // Never applied at all => NeedsReup.
        assert_eq!(reg.status("fresh"), Some(SourceStatus::NeedsReup));

        // applied_fingerprint None in container mode => NO source is applied.
        let reg_none =
            SourceRegistry::load(&config, &overr, Some(mount.path().to_path_buf()), None);
        assert_eq!(reg_none.status("docs"), Some(SourceStatus::NeedsReup));
    }

    #[test]
    fn golden_override_zero_sources() {
        let tmp = tempfile::tempdir().unwrap();
        let _reg = bare_registry(&tmp); // load() regenerates the override even when empty
        let got = std::fs::read_to_string(tmp.path().join(OVERRIDE_FILENAME)).unwrap();
        let want = "\
# GENERATED by idea-vault — do not edit. Managed from the Sources page (/sources).
# Apply changes: docker compose up -d   (the app never runs docker — ADR-0020)
services:
  idea-vault:
    environment:
      IDEA_VAULT_SOURCES_APPLIED: ''
";
        assert_eq!(got, want);
    }

    #[test]
    fn golden_override_one_source() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = bare_registry(&tmp);
        reg.add(source("docs", "/home/owner/docs")).unwrap();
        let got = std::fs::read_to_string(tmp.path().join(OVERRIDE_FILENAME)).unwrap();
        let want = "\
# GENERATED by idea-vault — do not edit. Managed from the Sources page (/sources).
# Apply changes: docker compose up -d   (the app never runs docker — ADR-0020)
services:
  idea-vault:
    environment:
      IDEA_VAULT_SOURCES_APPLIED: docs=/home/owner/docs
    volumes:
      - type: bind
        source: /home/owner/docs
        target: /mnt/sources/docs
        read_only: true
        bind:
          create_host_path: false
";
        assert_eq!(got, want);
    }

    #[test]
    fn golden_override_two_sources_sorted_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = bare_registry(&tmp);
        // Inserted out of order on purpose — the render must sort.
        reg.add(source("zeta", "/srv/zeta")).unwrap();
        reg.add(source("alpha", "/srv/alpha")).unwrap();
        let got = std::fs::read_to_string(tmp.path().join(OVERRIDE_FILENAME)).unwrap();
        let want = "\
# GENERATED by idea-vault — do not edit. Managed from the Sources page (/sources).
# Apply changes: docker compose up -d   (the app never runs docker — ADR-0020)
services:
  idea-vault:
    environment:
      IDEA_VAULT_SOURCES_APPLIED: alpha=/srv/alpha;zeta=/srv/zeta
    volumes:
      - type: bind
        source: /srv/alpha
        target: /mnt/sources/alpha
        read_only: true
        bind:
          create_host_path: false
      - type: bind
        source: /srv/zeta
        target: /mnt/sources/zeta
        read_only: true
        bind:
          create_host_path: false
";
        assert_eq!(got, want);
    }

    #[test]
    fn override_yaml_round_trips_through_a_yaml_parser() {
        // The golden strings pin the exact shape; this pins that the shape stays *valid* YAML
        // with the values intact (the property compose actually depends on).
        let tmp = tempfile::tempdir().unwrap();
        let reg = bare_registry(&tmp);
        reg.add(source("docs", "/home/owner/my docs")).unwrap(); // space: still a plain scalar
        let raw = std::fs::read_to_string(tmp.path().join(OVERRIDE_FILENAME)).unwrap();
        let parsed: serde_json::Value = serde_norway::from_str(&raw).unwrap();
        let svc = &parsed["services"]["idea-vault"];
        assert_eq!(
            svc["environment"]["IDEA_VAULT_SOURCES_APPLIED"],
            "docs=/home/owner/my docs"
        );
        assert_eq!(svc["volumes"][0]["source"], "/home/owner/my docs");
        assert_eq!(svc["volumes"][0]["target"], "/mnt/sources/docs");
        assert_eq!(svc["volumes"][0]["read_only"], true);
        assert_eq!(svc["volumes"][0]["bind"]["create_host_path"], false);
    }

    #[test]
    fn gitignore_is_created_with_both_dotfile_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let _reg = bare_registry(&tmp);
        let gitignore = std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
        let lines: Vec<&str> = gitignore.lines().collect();
        assert!(lines.contains(&".sources.json"), "{gitignore:?}");
        assert!(lines.contains(&OVERRIDE_FILENAME), "{gitignore:?}");
    }

    #[test]
    fn gitignore_appends_missing_lines_without_clobbering_owner_content() {
        let tmp = tempfile::tempdir().unwrap();
        // Owner content WITHOUT a trailing newline — the append must not glue lines together.
        std::fs::write(tmp.path().join(".gitignore"), "*.db\n.sources.json").unwrap();

        let reg = bare_registry(&tmp);
        let gitignore = std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
        let lines: Vec<&str> = gitignore.lines().collect();
        assert_eq!(
            lines,
            ["*.db", ".sources.json", OVERRIDE_FILENAME],
            "owner line kept, present line not duplicated, missing line appended cleanly"
        );

        // A mutation (which calls save → ensure) must not duplicate anything either.
        reg.add(source("docs", "/home/owner/docs")).unwrap();
        let again = std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
        assert_eq!(again.lines().collect::<Vec<_>>(), lines.as_slice());
    }
}
