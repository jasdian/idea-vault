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
    let mut files_scanned = 0usize;
    let mut stopped: Option<String> = None;

    let walker = WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        // depth 0 is the root itself — exempt it, its own name may legitimately start with '.'.
        .filter_entry(|e| e.depth() == 0 || !is_hidden(e.file_name()));
    'files: for entry in walker {
        // An unreadable subtree is skipped, not fatal — grep stays best-effort content.
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        if files_scanned == MAX_GREP_FILES {
            stopped = Some(format!(
                "…(scanned {MAX_GREP_FILES} files, stopped — narrow with source_list first)"
            ));
            break;
        }
        files_scanned += 1;
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
}
