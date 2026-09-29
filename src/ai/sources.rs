//! Deterministic reference-source tools (DRT leaves): read-only `list`/`grep`/`read` over the
//! owner's attached named sources, so the foil can consult reference material without ever
//! seeing — or choosing — a host filesystem path.
//!
//! The model picks *which* source (by name — an enum in the tool schema) and *what* to look
//! for; code resolves the name to a canonical root ([`crate::sources::ResolvedSource`]) and
//! every relative path funnels through [`resolve_rel`], the containment gate. Mirrors
//! [`super::web`]'s design rules:
//! - **Tool errors are content, not errors.** [`execute_tool`] never fails the turn — an escape
//!   attempt, a missing file, or an unattached source name comes back as a short text the model
//!   can read and route around (D20: degrade, don't die).
//! - **Bounded output.** Every leaf caps its result ([`MAX_LIST_ENTRIES`], [`MAX_GREP_MATCHES`],
//!   [`READ_MAX_CHARS`]) and announces truncation in-band, so one tool round can never blow the
//!   context budget.
//! - **Deterministic.** Directory walks are `sort_by_file_name`-ordered and hidden trees are
//!   skipped, so the same query over the same tree always returns the same text.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use serde_json::{json, Value};
use walkdir::WalkDir;

use crate::sources::ResolvedSource;

/// Max entries one `source_list` call returns before announcing truncation.
pub const MAX_LIST_ENTRIES: usize = 200;
/// Max files one `source_grep` call scans before giving up and telling the model to narrow.
pub const MAX_GREP_FILES: usize = 2_000;
/// Max matching lines one `source_grep` call returns.
pub const MAX_GREP_MATCHES: usize = 40;
/// Each grep hit line is truncated to this many characters (a minified asset line must not
/// swallow the whole result).
pub const GREP_LINE_MAX_CHARS: usize = 240;
/// Max characters of file text handed back from one `source_read` (parity with
/// `ai::web::FETCH_MAX_CHARS` — same "one tool round" budget).
pub const READ_MAX_CHARS: usize = 12_000;
/// Files larger than this are skipped by grep — reference *text* is small; anything bigger is
/// almost certainly an asset.
pub const MAX_SCAN_FILE_BYTES: u64 = 1_048_576;
/// How many leading bytes the binary sniff inspects for a NUL.
pub const BINARY_SNIFF_BYTES: usize = 8_192;
/// The logged argument summary is capped so a pathological query can't bloat the trace line.
const ARG_SUMMARY_MAX_CHARS: usize = 120;

/// The Ollama-shape tool definitions for the router's tool loop (`/api/chat` `tools` field).
/// The `source` property is a JSON-schema **enum of the attached source names** — the model
/// picks *which* source, never a path (DRT). With an empty slice the enum is empty; callers
/// should omit these tools entirely when the idea has no sources attached.
pub fn tool_definitions(sources: &[ResolvedSource]) -> Value {
    let names: Vec<&str> = sources.iter().map(|s| s.name.as_str()).collect();
    let source_prop = json!({
        "type": "string",
        "enum": names,
        "description": "name of the attached source to use"
    });
    json!([
        {
            "type": "function",
            "function": {
                "name": "source_list",
                "description": "List files in an attached read-only reference source. Returns \
                                relative paths (directories end with /). Use this first to \
                                orient yourself.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "source": source_prop,
                        "dir": {
                            "type": "string",
                            "description": "optional subdirectory (relative path) to list \
                                            instead of the source root"
                        }
                    },
                    "required": ["source"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "source_grep",
                "description": "Search an attached reference source for a literal text \
                                (case-insensitive; NOT a regex). Returns up to 40 matching \
                                lines as path:line: text.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "source": source_prop,
                        "query": {
                            "type": "string",
                            "description": "the literal text to search for"
                        }
                    },
                    "required": ["source", "query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "source_read",
                "description": "Read one file from an attached reference source (truncated to \
                                ~12000 characters). Use a path returned by source_list or \
                                source_grep.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "source": source_prop,
                        "file": {
                            "type": "string",
                            "description": "relative path of the file to read"
                        },
                        "offset_lines": {
                            "type": "integer",
                            "description": "1-based start line for continuing a truncated read"
                        }
                    },
                    "required": ["source", "file"]
                }
            }
        }
    ])
}

/// Execute one model-requested source tool call. Infallible by design: every failure mode
/// returns a short explanatory text the model can read and route around — a bad path or an
/// unattached source name must never turn a whole turn into `mark_failed`. All filesystem work
/// runs on the blocking pool ([`run_blocking`]).
pub async fn execute_tool(name: &str, args: &Value, sources: &[ResolvedSource]) -> String {
    // Ollama passes `function.arguments` as a JSON object; some models emit it as a string of
    // JSON instead — accept both (same tolerance as `ai::web::execute_tool`).
    let args = match args {
        Value::String(s) => serde_json::from_str::<Value>(s).unwrap_or(Value::Null),
        other => other.clone(),
    };
    if !matches!(name, "source_list" | "source_grep" | "source_read") {
        return format!("unknown tool: {name}");
    }
    // DRT: the model only names a source; code resolves it to a canonical root — or explains
    // what *is* attached, so the model can correct itself on the next round.
    let requested = args
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let Some(src) = sources.iter().find(|s| s.name == requested) else {
        if sources.is_empty() {
            return "no reference sources are attached to this idea".to_string();
        }
        let attached = sources
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return format!("unknown source '{requested}' — attached sources: {attached}");
    };

    let started = Instant::now();
    let root = src.root.clone();
    let (arg_summary, out) = match name {
        "source_list" => {
            let dir = args
                .get("dir")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from);
            let summary = dir.clone().unwrap_or_else(|| "(root)".to_string());
            let out = run_blocking(move || list_source(&root, dir.as_deref())).await;
            (summary, out)
        }
        "source_grep" => {
            let query = args
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if query.is_empty() {
                return "source_grep error: missing query".to_string();
            }
            let source_name = src.name.clone();
            let summary = query.clone();
            let out = run_blocking(move || grep_source(&root, &query, &source_name)).await;
            (summary, out)
        }
        "source_read" => {
            let file = args
                .get("file")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if file.is_empty() {
                return "source_read error: missing file".to_string();
            }
            // Some models emit numbers as strings — accept both shapes for the offset.
            let offset = args
                .get("offset_lines")
                .and_then(|v| {
                    v.as_u64()
                        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
                })
                .map(|n| n as usize);
            let summary = match offset {
                Some(o) => format!("{file} offset_lines={o}"),
                None => file.clone(),
            };
            let out = run_blocking(move || read_source_file(&root, &file, offset)).await;
            (summary, out)
        }
        _ => unreachable!("tool name validated above"),
    };
    // All in-band truncation notes start with "…(" — that marker doubles as the log signal.
    let truncated = out.contains("…(");
    tracing::info!(
        tool = name,
        source = %src.name,
        root = %src.root.display(),
        arg = %truncate_chars(&arg_summary, ARG_SUMMARY_MAX_CHARS),
        result_bytes = out.len(),
        truncated,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "source tool executed (drt leaf)"
    );
    out
}

/// Run one sync tool leaf on the blocking pool — a grep over a big source must never stall the
/// async runtime the web server shares. A join failure (panicked leaf) is still content.
async fn run_blocking(f: impl FnOnce() -> String + Send + 'static) -> String {
    match tokio::task::spawn_blocking(f).await {
        Ok(out) => out,
        Err(e) => format!("source tool task failed: {e}"),
    }
}

/// **The safety gate** for every relative path the model hands us — defense in depth, because
/// the tool schema only *suggests* relative paths, it cannot enforce them:
///
/// 1. Reject absolute paths and any `..` component **before** joining, so a hostile path never
///    reaches the filesystem at all.
/// 2. Join onto the source root and `std::fs::canonicalize` — this resolves symlinks (and, as a
///    side effect, requires the path to exist).
/// 3. Require the canonical result to still `starts_with(root)`. `root` is canonical by the
///    [`ResolvedSource`] invariant, so a symlink *inside* the source that points *outside* it
///    canonicalizes elsewhere and fails right here.
///
/// The error text tells the model what to do instead — it is tool content, never a panic.
fn resolve_rel(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let escape = || {
        format!(
            "path '{rel}' escapes the source root — use paths returned by source_list/source_grep"
        )
    };
    let rel_path = Path::new(rel);
    // Step 1: absolute paths and parent-dir hops are rejected before any filesystem contact.
    if rel_path.is_absolute()
        || rel_path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(escape());
    }
    // Step 2: join under the root, then canonicalize — resolves every symlink in the chain.
    let canonical = std::fs::canonicalize(root.join(rel_path)).map_err(|e| {
        format!("cannot open '{rel}': {e} — use paths returned by source_list/source_grep")
    })?;
    // Step 3: containment against the (canonical, by invariant) root catches symlink escapes.
    if !canonical.starts_with(root) {
        return Err(escape());
    }
    Ok(canonical)
}

/// A file is "binary" if its first [`BINARY_SNIFF_BYTES`] contain a NUL byte — the classic
/// git-style heuristic, good enough to keep mangled bytes out of the model's context.
fn looks_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(BINARY_SNIFF_BYTES)].contains(&0)
}

/// Hidden names (`.git`, `.obsidian`, …) are metadata, not reference text — and `.git` alone
/// can dwarf the real content, so both list and grep prune them.
fn is_hidden(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy().starts_with('.')
}

/// Case-insensitive **literal** substring search over every text file under `root`, in sorted
/// walk order. Output lines are `relpath:lineno: line` (line truncated to
/// [`GREP_LINE_MAX_CHARS`]); both caps announce themselves in-band so the model knows to narrow.
fn grep_source(root: &Path, query: &str, source: &str) -> String {
    let needle = query.to_lowercase();
    let mut matches: Vec<String> = Vec::new();
    let mut stopped: Option<String> = None;

    'files: for (files_scanned, entry) in source_files(root).enumerate() {
        if files_scanned == MAX_GREP_FILES {
            stopped = Some(format!(
                "…(scanned {MAX_GREP_FILES} files, stopped — narrow with source_list first)"
            ));
            break;
        }
        // Oversized files are assets, not reference text — skip without reading.
        if entry
            .metadata()
            .map(|m| m.len() > MAX_SCAN_FILE_BYTES)
            .unwrap_or(true)
        {
            continue;
        }
        let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
        };
        if looks_binary(&bytes) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        let rel = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .display()
            .to_string();
        for (idx, line) in text.lines().enumerate() {
            if line.to_lowercase().contains(&needle) {
                if matches.len() == MAX_GREP_MATCHES {
                    stopped = Some(format!(
                        "…(stopped at {MAX_GREP_MATCHES} matches — narrow the query)"
                    ));
                    break 'files;
                }
                matches.push(format!(
                    "{rel}:{}: {}",
                    idx + 1,
                    truncate_chars(line, GREP_LINE_MAX_CHARS)
                ));
            }
        }
    }

    let mut result = if matches.is_empty() {
        format!("no matches for '{query}' in {source}")
    } else {
        matches.join("\n")
    };
    if let Some(note) = stopped {
        result.push('\n');
        result.push_str(&note);
    }
    result
}

/// List (recursively, sorted, hidden trees pruned) up to [`MAX_LIST_ENTRIES`] entries under the
/// source root — or under `dir`, which goes through [`resolve_rel`] first. Paths are always
/// relative to the *source root* (so they feed straight into `source_read`); directories get a
/// trailing `/`.
fn list_source(root: &Path, dir: Option<&str>) -> String {
    let base = match dir {
        Some(d) => match resolve_rel(root, d) {
            Ok(p) => p,
            Err(e) => return e,
        },
        None => root.to_path_buf(),
    };
    let mut entries: Vec<String> = Vec::new();
    let mut truncated = false;
    let walker = WalkDir::new(&base)
        .follow_links(false)
        .min_depth(1)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !is_hidden(e.file_name()));
    for entry in walker {
        let Ok(entry) = entry else { continue };
        if entries.len() == MAX_LIST_ENTRIES {
            truncated = true;
            break;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .display()
            .to_string();
        if entry.file_type().is_dir() {
            entries.push(format!("{rel}/"));
        } else {
            entries.push(rel);
        }
    }
    if entries.is_empty() {
        return match dir {
            Some(d) => format!("'{d}' has no listable entries"),
            None => "the source has no listable entries".to_string(),
        };
    }
    let mut result = entries.join("\n");
    if truncated {
        result.push('\n');
        result.push_str(&format!(
            "…(stopped at {MAX_LIST_ENTRIES} entries — pass dir to list a subdirectory)"
        ));
    }
    result
}

/// Read one text file (path through [`resolve_rel`]), honoring a 1-based `offset_lines` and
/// capping at [`READ_MAX_CHARS`] — the truncation note names the exact `offset_lines` to
/// continue from, so a long file is read in resumable pages.
fn read_source_file(root: &Path, rel: &str, offset_lines: Option<usize>) -> String {
    let path = match resolve_rel(root, rel) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => return format!("cannot read '{rel}': {e}"),
    };
    if looks_binary(&bytes) {
        return format!("'{rel}' looks like a binary file — source_read only reads text files");
    }
    let text = String::from_utf8_lossy(&bytes);
    let total = text.lines().count();
    if total == 0 {
        return format!("'{rel}' is empty");
    }
    let offset = offset_lines.unwrap_or(1).max(1);
    if offset > total {
        return format!("offset_lines={offset} is past the end of '{rel}' ({total} lines)");
    }
    let mut out = String::new();
    let mut used = 0usize;
    let mut next: Option<usize> = None;
    for (idx, line) in text.lines().enumerate().skip(offset - 1) {
        let lineno = idx + 1;
        let cost = line.chars().count() + 1; // +1 for the newline we re-add
        if used + cost > READ_MAX_CHARS {
            if out.is_empty() {
                // A single line longer than the whole budget: cut it mid-line and point the
                // continuation *past* it, so a follow-up call always makes progress instead of
                // looping on the same line forever.
                out.push_str(&truncate_chars(line, READ_MAX_CHARS));
                out.push('\n');
                next = Some(lineno + 1);
            } else {
                next = Some(lineno);
            }
            break;
        }
        out.push_str(line);
        out.push('\n');
        used += cost;
    }
    if let Some(n) = next {
        if n <= total {
            out.push_str(&format!(
                "…(truncated — call source_read again with offset_lines={n} to continue)"
            ));
        }
    }
    out
}

/// Truncate to at most `max` characters on a char boundary (budgets here are characters of
/// text handed to the model, not bytes on disk — same rule as `ai::web`).
fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((idx, _)) => format!("{}…", &s[..idx]),
        None => s.to_string(),
    }
}

/// Most bytes one [`SourceProbe`] walk reads across all attached sources before it stops.
pub const PROBE_MAX_TOTAL_BYTES: u64 = 20 * 1_048_576;

/// What a [`SourceProbe`] found for a `path:line` anchor paired with a symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnchorCheck {
    /// The symbol is on the cited lines of the one file the path resolves to.
    Resolved { source: String, path: String },
    /// The file exists but the symbol sits on another line (the first one it occurs on).
    Moved {
        source: String,
        path: String,
        line: usize,
    },
    /// The file exists but never mentions the symbol.
    SymbolMissing { source: String, path: String },
    /// No attached source has a file at (or uniquely ending in) this path, and the walk was
    /// complete.
    NoFile,
    /// The path suffix-matches more than one file; the candidates, as `source:path`.
    Ambiguous(Vec<String>),
    /// Nothing could be settled: no source is attached, the symbol is empty, the file is not
    /// bounded text (binary, oversized, unreadable), or the walk hit its cap before the path was
    /// found or shown unique.
    Unverified,
}

/// Which tokens a [`SourceProbe`] found; `complete` is false when the walk hit its cap or passed
/// an oversized file it never reads, so a token outside `found` is unknown rather than absent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenScan {
    pub found: BTreeSet<String>,
    pub complete: bool,
}

/// Every text-candidate file under `root`: hidden names pruned (the root itself exempt), symlinks
/// never followed, sorted walk order. Shared by `source_grep` and [`SourceProbe`].
fn source_files(root: &Path) -> impl Iterator<Item = walkdir::DirEntry> {
    WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        // depth 0 is the root itself — exempt it, its own name may legitimately start with '.'.
        .filter_entry(|e| e.depth() == 0 || !is_hidden(e.file_name()))
        // An unreadable subtree is skipped, not fatal.
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
}

/// True when `needle` occurs in `hay` with no identifier character glued to either end, so `id`
/// does not match inside `idea`. An edge of `needle` that is itself punctuation needs no boundary.
fn contains_word(hay: &str, needle: &str) -> bool {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    let (Some(head), Some(tail)) = (needle.chars().next(), needle.chars().last()) else {
        return false;
    };
    hay.match_indices(needle).any(|(at, _)| {
        let before = hay[..at].chars().next_back();
        let after = hay[at + needle.len()..].chars().next();
        let glued_before = ident(head) && before.is_some_and(ident);
        let glued_after = ident(tail) && after.is_some_and(ident);
        !glued_before && !glued_after
    })
}

/// The files one probe walk visited, and whether the walk stopped at a cap.
#[derive(Debug, Clone, Default)]
struct Walk {
    files: Vec<(String, String, PathBuf)>,
    truncated: bool,
    /// A file over [`MAX_SCAN_FILE_BYTES`] was listed but will never be read.
    oversized: bool,
}

/// A read-only, bounded view over the reference sources attached to one idea (ADR-0021), used
/// by the build-plan gates (docs/adr/0030) to check that a cited file and symbol exist. It never
/// runs a command and never leaves a source root: exact paths go through [`resolve_rel`], suffix
/// matches come from the same hidden-pruned, symlink-free walk as `source_grep` (done once per
/// probe, capped at [`MAX_GREP_FILES`] files and [`PROBE_MAX_TOTAL_BYTES`]), and no file larger
/// than [`MAX_SCAN_FILE_BYTES`] is read. Symbols and tokens match case-sensitively on identifier
/// boundaries. Every method is blocking file I/O — call it from `spawn_blocking`.
#[derive(Debug, Clone, Default)]
pub struct SourceProbe {
    roots: Vec<(String, PathBuf)>,
    max_files: usize,
    walk: std::sync::OnceLock<Walk>,
}

impl SourceProbe {
    /// A probe over the given resolved sources (their roots are canonical by invariant).
    pub fn new(sources: &[ResolvedSource]) -> Self {
        SourceProbe {
            roots: sources
                .iter()
                .map(|s| (s.name.to_string(), s.root.clone()))
                .collect(),
            max_files: MAX_GREP_FILES,
            walk: std::sync::OnceLock::new(),
        }
    }

    /// True when no source is attached — every check then reports [`AnchorCheck::Unverified`].
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// Every text-candidate file under every root, as (source, root-relative path, absolute
    /// path), within the probe's caps — walked on first use and reused after.
    fn walk(&self) -> &Walk {
        self.walk.get_or_init(|| {
            let mut walk = Walk::default();
            let mut bytes = 0u64;
            for (name, root) in &self.roots {
                for entry in source_files(root) {
                    let len = entry.metadata().map(|m| m.len()).unwrap_or(u64::MAX);
                    // An oversized file stays listed (a matching anchor is then Unverified,
                    // not NoFile) but costs none of the byte budget, since it is never read.
                    let oversized = len > MAX_SCAN_FILE_BYTES;
                    let cost = if oversized { 0 } else { len };
                    if walk.files.len() == self.max_files || bytes + cost > PROBE_MAX_TOTAL_BYTES {
                        walk.truncated = true;
                        return walk;
                    }
                    bytes += cost;
                    walk.oversized |= oversized;
                    let rel = entry
                        .path()
                        .strip_prefix(root)
                        .unwrap_or(entry.path())
                        .to_string_lossy()
                        .into_owned();
                    walk.files
                        .push((name.clone(), rel, entry.path().to_path_buf()));
                }
            }
            walk
        })
    }

    /// The file's text, or `None` when it is larger than [`MAX_SCAN_FILE_BYTES`] (checked
    /// while reading, so a file growing mid-read stays bounded), binary, or unreadable.
    fn read_text(path: &Path) -> Option<String> {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .ok()?
            .take(MAX_SCAN_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        (bytes.len() as u64 <= MAX_SCAN_FILE_BYTES && !looks_binary(&bytes))
            .then(|| String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Check that `path` (root-relative, or a unique path suffix) names a file whose lines
    /// `first..=last` (1-based; a reversed range is read in order, line 0 as line 1) contain
    /// `symbol`. A path with a hidden component is never opened and reports `Unverified`.
    pub fn check_anchor(&self, path: &str, first: usize, last: usize, symbol: &str) -> AnchorCheck {
        if self.roots.is_empty() || symbol.trim().is_empty() {
            return AnchorCheck::Unverified;
        }
        let path = path.trim().trim_start_matches("./");
        if Path::new(path)
            .components()
            .any(|c| matches!(c, Component::Normal(n) if is_hidden(n)))
        {
            return AnchorCheck::Unverified;
        }
        let exact: Vec<(String, String, PathBuf)> = self
            .roots
            .iter()
            .filter_map(|(name, root)| {
                let abs = resolve_rel(root, path).ok()?;
                let rel = abs.strip_prefix(root).ok()?.to_string_lossy().into_owned();
                abs.is_file().then(|| (name.clone(), rel, abs))
            })
            .collect();
        let (hits, truncated) = if exact.is_empty() {
            let suffix = format!("/{path}");
            let walk = self.walk();
            let hits: Vec<_> = walk
                .files
                .iter()
                .filter(|(_, rel, _)| rel.ends_with(&suffix))
                .cloned()
                .collect();
            (hits, walk.truncated)
        } else {
            (exact, false)
        };
        match hits.as_slice() {
            [] | [_] if truncated => AnchorCheck::Unverified,
            [] => AnchorCheck::NoFile,
            [(source, rel, abs)] => {
                let Some(text) = Self::read_text(abs) else {
                    return AnchorCheck::Unverified;
                };
                let lines: Vec<&str> = text.lines().collect();
                let (lo, hi) = (first.min(last).max(1), first.max(last).max(1));
                let cited = lines
                    .get(lo - 1..hi.min(lines.len()))
                    .is_some_and(|span| span.iter().any(|l| contains_word(l, symbol)));
                let (source, path) = (source.clone(), rel.clone());
                if cited {
                    AnchorCheck::Resolved { source, path }
                } else if let Some(i) = lines.iter().position(|l| contains_word(l, symbol)) {
                    AnchorCheck::Moved {
                        source,
                        path,
                        line: i + 1,
                    }
                } else {
                    AnchorCheck::SymbolMissing { source, path }
                }
            }
            many => AnchorCheck::Ambiguous(
                many.iter()
                    .map(|(source, rel, _)| format!("{source}:{rel}"))
                    .collect(),
            ),
        }
    }

    /// Whether `path` (root-relative or a unique-or-not path suffix, a file or a directory)
    /// exists in an attached source. `None` when no source is attached, the path has a hidden
    /// component, or the walk hit its cap before the path turned up.
    pub fn has_path(&self, path: &str) -> Option<bool> {
        let path = path.trim().trim_start_matches("./").trim_end_matches('/');
        if self.roots.is_empty()
            || path.is_empty()
            || Path::new(path)
                .components()
                .any(|c| matches!(c, Component::Normal(n) if is_hidden(n)))
        {
            return None;
        }
        if self
            .roots
            .iter()
            .any(|(_, root)| resolve_rel(root, path).is_ok())
        {
            return Some(true);
        }
        let walk = self.walk();
        let (suffix, inside, below) = (format!("/{path}"), format!("/{path}/"), format!("{path}/"));
        let hit = walk.files.iter().any(|(_, rel, _)| {
            rel == path
                || rel.ends_with(&suffix)
                || rel.starts_with(&below)
                || rel.contains(&inside)
        });
        match (hit, walk.truncated) {
            (true, _) => Some(true),
            (false, true) => None,
            (false, false) => Some(false),
        }
    }

    /// Which of `tokens` occur in any attached source file, over the probe's one bounded walk.
    /// Blank tokens are ignored. With no source attached nothing is found and the scan is
    /// incomplete.
    pub fn find_tokens(&self, tokens: &[String]) -> TokenScan {
        let wanted: BTreeSet<&str> = tokens
            .iter()
            .map(|t| t.as_str())
            .filter(|t| !t.trim().is_empty())
            .collect();
        if self.roots.is_empty() {
            return TokenScan::default();
        }
        let mut found = BTreeSet::new();
        let walk = self.walk();
        for (_, _, abs) in &walk.files {
            if found.len() == wanted.len() {
                break;
            }
            let Some(text) = Self::read_text(abs) else {
                continue;
            };
            for token in &wanted {
                if !found.contains(*token) && contains_word(&text, token) {
                    found.insert(token.to_string());
                }
            }
        }
        TokenScan {
            complete: !(walk.truncated || walk.oversized) || found.len() == wanted.len(),
            found,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Name;

    /// A canonical tempdir with a small fixture tree: a root file with a known match line, a
    /// subdirectory, a hidden directory (must stay invisible), and a binary file whose bytes
    /// contain the needle (must be skipped, not matched).
    fn fixture_root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonicalize tempdir");
        std::fs::write(
            root.join("alpha.md"),
            "First line\nsecond LINE with Needle\nthird\n",
        )
        .expect("write alpha.md");
        std::fs::create_dir(root.join("sub")).expect("mkdir sub");
        std::fs::write(root.join("sub").join("beta.txt"), "needle in sub\n")
            .expect("write beta.txt");
        std::fs::create_dir(root.join(".hidden")).expect("mkdir .hidden");
        std::fs::write(root.join(".hidden").join("secret.md"), "needle hidden\n")
            .expect("write secret.md");
        std::fs::write(root.join("bin.dat"), b"\x00\x01needle\x00").expect("write bin.dat");
        (dir, root)
    }

    fn source(root: &Path) -> ResolvedSource {
        ResolvedSource {
            name: Name::try_from("notes").unwrap(),
            root: root.to_path_buf(),
        }
    }

    #[test]
    fn resolve_rel_rejects_absolute_and_parent_before_touching_disk() {
        let (_dir, root) = fixture_root();
        for bad in ["/etc/passwd", "../outside", "sub/../../outside"] {
            let err = resolve_rel(&root, bad).expect_err("must be rejected");
            assert!(err.contains("escapes"), "{bad} → {err}");
        }
    }

    #[test]
    fn resolve_rel_happy_path_and_missing_file() {
        let (_dir, root) = fixture_root();
        let ok = resolve_rel(&root, "sub/beta.txt").expect("existing file resolves");
        assert_eq!(ok, root.join("sub").join("beta.txt"));
        let err = resolve_rel(&root, "no-such.md").expect_err("missing file is an error");
        assert!(err.contains("cannot open"), "got: {err}");
    }

    #[cfg(unix)]
    #[test]
    fn resolve_rel_blocks_symlink_escape() {
        let (_dir, root) = fixture_root();
        let outside = tempfile::tempdir().expect("outside tempdir");
        let target = outside.path().join("outside.md");
        std::fs::write(&target, "outside truth").expect("write outside file");
        std::os::unix::fs::symlink(&target, root.join("link.md")).expect("symlink");
        // The link lives *inside* the root, but canonicalizes outside it — step 3 must catch it.
        let err = resolve_rel(&root, "link.md").expect_err("symlink escape must fail");
        assert!(err.contains("escapes"), "got: {err}");
    }

    #[test]
    fn grep_finds_matches_with_rel_path_and_lineno_case_insensitive() {
        let (_dir, root) = fixture_root();
        let out = grep_source(&root, "NEEDLE", "notes");
        assert!(
            out.contains("alpha.md:2: second LINE with Needle"),
            "got: {out}"
        );
        assert!(out.contains("sub/beta.txt:1: needle in sub"), "got: {out}");
        assert!(!out.contains("hidden"), "dot-dirs stay invisible: {out}");
        assert!(!out.contains("bin.dat"), "binary files are skipped: {out}");
    }

    #[test]
    fn grep_no_matches_is_readable_content() {
        let (_dir, root) = fixture_root();
        assert_eq!(
            grep_source(&root, "zzz-not-here", "notes"),
            "no matches for 'zzz-not-here' in notes"
        );
    }

    #[test]
    fn grep_match_cap_announces_itself() {
        let (_dir, root) = fixture_root();
        let many = (0..50)
            .map(|i| format!("capneedle {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(root.join("caps.md"), many).expect("write caps.md");
        let out = grep_source(&root, "capneedle", "notes");
        let match_lines = out.lines().filter(|l| l.contains("caps.md:")).count();
        assert_eq!(match_lines, MAX_GREP_MATCHES);
        assert!(
            out.ends_with(&format!(
                "…(stopped at {MAX_GREP_MATCHES} matches — narrow the query)"
            )),
            "got: {out}"
        );
    }

    #[test]
    fn grep_is_deterministic() {
        let (_dir, root) = fixture_root();
        let a = grep_source(&root, "needle", "notes");
        let b = grep_source(&root, "needle", "notes");
        assert_eq!(a, b);
    }

    #[test]
    fn list_root_and_subdir_hide_dot_dirs() {
        let (_dir, root) = fixture_root();
        let out = list_source(&root, None);
        assert!(out.contains("alpha.md"), "got: {out}");
        assert!(out.contains("sub/\n"), "dirs get a trailing slash: {out}");
        assert!(out.contains("sub/beta.txt"), "got: {out}");
        assert!(!out.contains(".hidden"), "dot-dirs stay invisible: {out}");
        // A narrowed listing still returns root-relative paths (they feed source_read).
        assert_eq!(list_source(&root, Some("sub")), "sub/beta.txt");
        // dir goes through the same safety gate as every other path.
        let err = list_source(&root, Some("../x"));
        assert!(err.contains("escapes"), "got: {err}");
    }

    #[test]
    fn list_cap_announces_itself() {
        let (_dir, root) = fixture_root();
        std::fs::create_dir(root.join("many")).expect("mkdir many");
        for i in 0..(MAX_LIST_ENTRIES + 10) {
            std::fs::write(root.join("many").join(format!("f{i:04}.md")), "x")
                .expect("write filler");
        }
        let out = list_source(&root, Some("many"));
        assert_eq!(out.lines().count(), MAX_LIST_ENTRIES + 1, "cap + note");
        assert!(
            out.ends_with(&format!(
                "…(stopped at {MAX_LIST_ENTRIES} entries — pass dir to list a subdirectory)"
            )),
            "got: {out}"
        );
    }

    #[test]
    fn read_happy_offset_and_past_end() {
        let (_dir, root) = fixture_root();
        assert_eq!(
            read_source_file(&root, "alpha.md", None),
            "First line\nsecond LINE with Needle\nthird\n"
        );
        assert_eq!(
            read_source_file(&root, "alpha.md", Some(2)),
            "second LINE with Needle\nthird\n"
        );
        let out = read_source_file(&root, "alpha.md", Some(99));
        assert!(out.contains("past the end"), "got: {out}");
    }

    #[test]
    fn read_truncation_hint_names_the_continuation_offset() {
        let (_dir, root) = fixture_root();
        // 300 lines of exactly 100 chars (+1 newline each = 101): 118 lines fit the 12 000-char
        // budget (118 × 101 = 11 918), so the continuation offset must be line 119.
        let body = (1..=300)
            .map(|i| format!("{i:04} {}", "x".repeat(95)))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(root.join("long.md"), body).expect("write long.md");
        let first = read_source_file(&root, "long.md", None);
        assert!(first.contains("0001 "), "starts at line 1: {first}");
        assert!(
            first.ends_with(
                "…(truncated — call source_read again with offset_lines=119 to continue)"
            ),
            "got tail: {}",
            &first[first.len().saturating_sub(120)..]
        );
        // Continuing from the named offset picks up exactly where the first read stopped.
        let second = read_source_file(&root, "long.md", Some(119));
        assert!(second.starts_with("0119 "), "got: {second}");
    }

    #[test]
    fn read_refuses_binary() {
        let (_dir, root) = fixture_root();
        let out = read_source_file(&root, "bin.dat", None);
        assert!(out.contains("binary"), "got: {out}");
    }

    #[tokio::test]
    async fn execute_tool_unknown_source_texts() {
        let (_dir, root) = fixture_root();
        // No sources attached at all: the model is told so, regardless of the name it guessed.
        assert_eq!(
            execute_tool("source_list", &json!({"source": "x"}), &[]).await,
            "no reference sources are attached to this idea"
        );
        // Wrong name with sources attached: the error lists what IS attached.
        let sources = vec![
            source(&root),
            ResolvedSource {
                name: Name::try_from("extra").unwrap(),
                root: root.clone(),
            },
        ];
        assert_eq!(
            execute_tool(
                "source_grep",
                &json!({"source": "x", "query": "q"}),
                &sources
            )
            .await,
            "unknown source 'x' — attached sources: notes, extra"
        );
    }

    #[tokio::test]
    async fn execute_tool_is_infallible_content() {
        let (_dir, root) = fixture_root();
        let sources = vec![source(&root)];
        assert_eq!(
            execute_tool("nope", &json!({}), &sources).await,
            "unknown tool: nope"
        );
        assert_eq!(
            execute_tool("source_grep", &json!({"source": "notes"}), &sources).await,
            "source_grep error: missing query"
        );
        assert_eq!(
            execute_tool("source_read", &json!({"source": "notes"}), &sources).await,
            "source_read error: missing file"
        );
        // A model-invented escape path comes back as routable text, never Err.
        let out = execute_tool(
            "source_read",
            &json!({"source": "notes", "file": "../secret"}),
            &sources,
        )
        .await;
        assert!(out.contains("escapes"), "got: {out}");
    }

    #[tokio::test]
    async fn execute_tool_accepts_string_encoded_args_and_dispatches() {
        let (_dir, root) = fixture_root();
        let sources = vec![source(&root)];
        // String-encoded arguments (some models) are accepted, same as ai::web.
        let out = execute_tool(
            "source_grep",
            &json!("{\"source\": \"notes\", \"query\": \"needle\"}"),
            &sources,
        )
        .await;
        assert!(out.contains("alpha.md:2:"), "got: {out}");
        // And a normal object-args read round-trips through spawn_blocking.
        let out = execute_tool(
            "source_read",
            &json!({"source": "notes", "file": "alpha.md"}),
            &sources,
        )
        .await;
        assert!(out.starts_with("First line"), "got: {out}");
    }

    #[test]
    fn tool_definitions_embed_the_source_enum() {
        let (_dir, root) = fixture_root();
        let defs = tool_definitions(&[source(&root)]);
        let tools = defs.as_array().expect("array of tools");
        assert_eq!(tools.len(), 3);
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t["function"]["name"].as_str().expect("name"))
            .collect();
        assert_eq!(names, ["source_list", "source_grep", "source_read"]);
        for t in tools {
            assert_eq!(
                t["function"]["parameters"]["properties"]["source"]["enum"],
                json!(["notes"]),
                "every tool constrains `source` to the attached names (DRT)"
            );
        }
    }

    fn code_root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonicalize tempdir");
        std::fs::create_dir_all(root.join("risk/src")).unwrap();
        std::fs::write(
            root.join("risk/src/calculator.rs"),
            "use x;\n\npub fn calculate_regime_factor() {}\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        std::fs::write(root.join("a/mod.rs"), "fn one() {}\n").unwrap();
        std::fs::write(root.join("b/mod.rs"), "fn two() {}\n").unwrap();
        (dir, root)
    }

    fn at(path: &str) -> (String, String) {
        ("notes".to_string(), path.to_string())
    }

    fn resolved(path: &str) -> AnchorCheck {
        let (source, path) = at(path);
        AnchorCheck::Resolved { source, path }
    }

    #[test]
    fn probe_reports_resolved_moved_missing_anchors() {
        let (_dir, root) = code_root();
        let probe = SourceProbe::new(&[source(&root)]);
        let path = "risk/src/calculator.rs";
        assert_eq!(
            probe.check_anchor(path, 3, 3, "calculate_regime_factor"),
            resolved(path)
        );
        let (source, p) = at(path);
        assert_eq!(
            probe.check_anchor(path, 1, 2, "calculate_regime_factor"),
            AnchorCheck::Moved {
                source: source.clone(),
                path: p.clone(),
                line: 3
            }
        );
        assert_eq!(
            probe.check_anchor(path, 1, 3, "load_context"),
            AnchorCheck::SymbolMissing { source, path: p }
        );
        assert_eq!(
            probe.check_anchor("risk/src/budget.rs", 1, 1, "x"),
            AnchorCheck::NoFile
        );
    }

    #[test]
    fn probe_line_range_edges() {
        let (_dir, root) = code_root();
        let probe = SourceProbe::new(&[source(&root)]);
        let path = "risk/src/calculator.rs";
        let sym = "calculate_regime_factor";
        assert_eq!(
            probe.check_anchor(path, 2, 3, sym),
            resolved(path),
            "last cited line"
        );
        assert_eq!(
            probe.check_anchor(path, 2, 1, "x"),
            resolved(path),
            "reversed range"
        );
        assert_eq!(
            probe.check_anchor(path, 0, 3, sym),
            resolved(path),
            "line 0 reads as 1"
        );
        assert_eq!(
            probe.check_anchor(path, 3, 99, sym),
            resolved(path),
            "past the end"
        );
        assert_eq!(
            probe.check_anchor(path, 1, usize::MAX, sym),
            resolved(path),
            "a huge line number never overflows"
        );
        assert!(matches!(
            probe.check_anchor(path, 50, 60, sym),
            AnchorCheck::Moved { line: 3, .. }
        ));
    }

    #[test]
    fn probe_matches_on_identifier_boundaries() {
        let (_dir, root) = code_root();
        let probe = SourceProbe::new(&[source(&root)]);
        let path = "risk/src/calculator.rs";
        assert!(matches!(
            probe.check_anchor(path, 3, 3, "regime"),
            AnchorCheck::SymbolMissing { .. }
        ));
        assert_eq!(probe.check_anchor(path, 3, 3, ""), AnchorCheck::Unverified);
        assert_eq!(
            probe.check_anchor(path, 3, 3, "fn calculate_regime_factor()"),
            resolved(path)
        );
        let scan = probe.find_tokens(&["one".into(), "on".into(), " ".into()]);
        assert_eq!(scan.found.into_iter().collect::<Vec<_>>(), ["one"]);
        assert!(scan.complete);
    }

    #[test]
    fn probe_suffix_matches_only_a_unique_path() {
        let (_dir, root) = code_root();
        let probe = SourceProbe::new(&[source(&root)]);
        assert_eq!(
            probe.check_anchor("src/calculator.rs", 3, 3, "calculate_regime_factor"),
            resolved("risk/src/calculator.rs")
        );
        assert_eq!(
            probe.check_anchor("mod.rs", 1, 1, "fn"),
            AnchorCheck::Ambiguous(vec!["notes:a/mod.rs".into(), "notes:b/mod.rs".into()])
        );
    }

    #[test]
    fn probe_reports_the_opened_path() {
        let (_dir, root) = code_root();
        let probe = SourceProbe::new(&[source(&root)]);
        assert_eq!(
            probe.check_anchor("risk//src/calculator.rs", 3, 3, "calculate_regime_factor"),
            resolved("risk/src/calculator.rs")
        );
    }

    #[test]
    fn probe_leaves_hidden_paths_unverified() {
        let (_dir, root) = fixture_root();
        let probe = SourceProbe::new(&[source(&root)]);
        assert_eq!(
            probe.check_anchor(".hidden/secret.md", 1, 1, "needle"),
            AnchorCheck::Unverified,
            "refusing to look is not a miss"
        );
    }

    #[test]
    fn probe_leaves_unreadable_files_unverified() {
        let (_dir, root) = fixture_root();
        std::fs::write(
            root.join("big.log"),
            vec![b'a'; MAX_SCAN_FILE_BYTES as usize + 1],
        )
        .unwrap();
        std::fs::write(
            root.join("sub/huge.log"),
            [
                b"large_only_token ".as_slice(),
                &vec![b'a'; MAX_SCAN_FILE_BYTES as usize],
            ]
            .concat(),
        )
        .unwrap();
        let probe = SourceProbe::new(&[source(&root)]);
        assert_eq!(
            probe.check_anchor("huge.log", 1, 1, "large_only_token"),
            AnchorCheck::Unverified,
            "a suffix match on an oversized file is not a miss"
        );
        let scan = probe.find_tokens(&["large_only_token".to_string()]);
        assert!(scan.found.is_empty());
        assert!(
            !scan.complete,
            "an unread oversized file leaves absent tokens unknown"
        );
        assert!(probe.find_tokens(&["Needle".to_string()]).complete);
        assert_eq!(
            probe.check_anchor("bin.dat", 1, 1, "needle"),
            AnchorCheck::Unverified
        );
        assert_eq!(
            probe.check_anchor("big.log", 1, 1, "a"),
            AnchorCheck::Unverified
        );
    }

    #[cfg(unix)]
    #[test]
    fn probe_rejects_escapes_and_symlinks_out() {
        let (dir, root) = code_root();
        let outside = tempfile::Builder::new()
            .prefix("outside")
            .tempdir_in(dir.path().parent().unwrap())
            .unwrap();
        let outside_root = outside.path().canonicalize().unwrap();
        std::fs::write(outside_root.join("secret.rs"), "fn leaked() {}\n").unwrap();
        std::os::unix::fs::symlink(outside_root.join("secret.rs"), root.join("risk/link.rs"))
            .unwrap();
        let probe = SourceProbe::new(&[source(&root)]);
        let escaping = format!(
            "../{}/secret.rs",
            outside_root.file_name().unwrap().to_str().unwrap()
        );
        assert!(
            root.join(&escaping).is_file(),
            "the escaping path names a real file"
        );
        assert_eq!(
            probe.check_anchor(&escaping, 1, 1, "leaked"),
            AnchorCheck::NoFile
        );
        assert_eq!(
            probe.check_anchor(
                outside_root.join("secret.rs").to_str().unwrap(),
                1,
                1,
                "leaked"
            ),
            AnchorCheck::NoFile
        );
        assert_eq!(
            probe.check_anchor("link.rs", 1, 1, "leaked"),
            AnchorCheck::NoFile
        );
        assert!(probe.find_tokens(&["leaked".to_string()]).found.is_empty());
    }

    #[test]
    fn probe_finds_tokens_in_one_bounded_walk() {
        let (_dir, root) = fixture_root();
        std::fs::write(root.join("only.bin"), b"\x00binary_only_token\x00").unwrap();
        let probe = SourceProbe::new(&[source(&root)]);
        let scan = probe.find_tokens(&[
            "Needle".to_string(),
            "Needle".to_string(),
            "NEEDLE".to_string(),
            "needle hidden".to_string(),
            "binary_only_token".to_string(),
            "absent_token".to_string(),
        ]);
        assert_eq!(scan.found.into_iter().collect::<Vec<_>>(), ["Needle"]);
        assert!(scan.complete);
    }

    #[test]
    fn probe_at_its_cap_never_reports_a_miss() {
        let (_dir, root) = code_root();
        let mut probe = SourceProbe::new(&[source(&root)]);
        probe.max_files = 1;
        assert_eq!(
            probe.check_anchor("b/mod.rs", 1, 1, "two"),
            resolved("b/mod.rs"),
            "an exact path does not need the walk"
        );
        assert_eq!(
            probe.check_anchor("mod.rs", 1, 1, "fn"),
            AnchorCheck::Unverified
        );
        assert_eq!(
            probe.check_anchor("gone.rs", 1, 1, "fn"),
            AnchorCheck::Unverified
        );
        let scan = probe.find_tokens(&["two".to_string()]);
        assert!(scan.found.is_empty());
        assert!(!scan.complete, "a capped walk leaves absent tokens unknown");
    }

    #[test]
    fn probe_has_path_sees_files_and_directories() {
        let (_dir, root) = code_root();
        let probe = SourceProbe::new(&[source(&root)]);
        assert_eq!(probe.has_path("a/mod.rs"), Some(true));
        assert_eq!(probe.has_path("src/calculator.rs"), Some(true));
        assert_eq!(probe.has_path("risk/src/"), Some(true));
        assert_eq!(probe.has_path("gone/mod.rs"), Some(false));
        assert_eq!(probe.has_path("/etc/passwd"), Some(false));
        assert_eq!(probe.has_path(".git/config"), None);
        assert_eq!(SourceProbe::default().has_path("a/mod.rs"), None);
        let mut capped = SourceProbe::new(&[source(&root)]);
        capped.max_files = 1;
        assert_eq!(
            capped.has_path("gone/mod.rs"),
            None,
            "a capped walk is unknown"
        );
    }

    #[test]
    fn probe_without_sources_is_unverified() {
        let probe = SourceProbe::default();
        assert!(probe.is_empty());
        assert_eq!(
            probe.check_anchor("any.rs", 1, 1, "x"),
            AnchorCheck::Unverified
        );
        let scan = probe.find_tokens(&["x".to_string()]);
        assert!(scan.found.is_empty());
        assert!(!scan.complete);
    }
}
