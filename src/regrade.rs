//! Parser corpus and read-only regrade (docs/adr/0038, D40).
//!
//! Every deterministic judge of a model answer (the audit parser, the store-time facts parser and
//! its evidence gate, the skill output contracts, the build-plan parser and gates) writes one
//! canonical verdict line through [`summarize`]. The run journal (docs/adr/0037) records that line
//! beside the call's verbatim text, and the committed parser corpus records it for curated
//! fixtures. Replaying today's parser over the same text and comparing lines is how a parse,
//! detector or gate change shows what it flips, with no model call.
//!
//! Replay covers parse, detector and gate code only: a prompt change alters what the model would
//! say, which no recorded answer can show. `regrade` never writes to the vault; the only write is
//! `--export`, which the owner runs by hand to copy one answer into the repo's corpus (nothing is
//! ever exported automatically, owner decision 2026-09-30).
//!
//! A bin-level module like `import`: it reads the vault and calls into `ai`, `concepts` and
//! `memory`, and no library module depends on it (D4). The parse sites call their own module's
//! summary function; [`summarize`] only dispatches to those, so the two cannot drift.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::ai::contract;
use crate::ai::journal::{self, JournalEntry};
use crate::ai::verdict::{sha256_hex, Haystack, HaystackRef, ParserKind};
use crate::concepts::audit;
use crate::concepts::build_plan::finish;
use crate::domain::{slug, OutputContract};
use crate::memory::extract;
use crate::vault::store;

/// Where `--export` writes, relative to the working directory (the repo root).
pub const CORPUS_DIR: &str = "tests/fixtures/raw-outputs";

/// The canonical one-line verdict of `kind` over `raw`. Space-separated `key=value` tokens, the
/// first always `pass=<n>`: how many units the parser accepted, which orders a flip as towards
/// passing or failing.
pub fn summarize(kind: &ParserKind, raw: &str, haystack: Option<Haystack>) -> String {
    match kind {
        ParserKind::Audit { n } => audit::summarize_audit(raw, *n),
        ParserKind::Facts => extract::summarize_facts(raw, haystack),
        ParserKind::Contract { name } => match contract_named(name) {
            Some(c) => contract::summarize_contract(c, raw),
            None => format!("pass=0 unknown_contract={name}"),
        },
        ParserKind::PlanGates => finish::summarize_plan_gates(raw, haystack),
    }
}

/// The contract whose frontmatter spelling is `name`.
fn contract_named(name: &str) -> Option<OutputContract> {
    serde_json::from_value(serde_json::Value::String(name.to_string())).ok()
}

/// The `pass=<n>` of a verdict line.
fn pass_of(summary: &str) -> Option<i64> {
    summary
        .split_whitespace()
        .find_map(|t| t.strip_prefix("pass="))
        .and_then(|n| n.parse().ok())
}

/// The tokens that differ between two verdict lines, as `key before->after` (`∅` for a token
/// only one side has), in the order they first appear.
pub fn flip_detail(before: &str, after: &str) -> String {
    let split = |s: &str| -> Vec<(String, String)> {
        s.split_whitespace()
            .map(|t| match t.split_once('=') {
                Some((k, v)) => (k.to_string(), v.to_string()),
                None => (t.to_string(), String::new()),
            })
            .collect()
    };
    let (b, a) = (split(before), split(after));
    let mut keys: Vec<&str> = Vec::new();
    for (k, _) in b.iter().chain(&a) {
        if !keys.contains(&k.as_str()) {
            keys.push(k);
        }
    }
    let get = |side: &[(String, String)], k: &str| {
        side.iter()
            .find(|(key, _)| key == k)
            .map_or("∅".to_string(), |(_, v)| v.clone())
    };
    keys.into_iter()
        .filter(|k| *k != "pass")
        .filter_map(|k| {
            let (x, y) = (get(&b, k), get(&a, k));
            (x != y).then(|| format!("{k} {x}->{y}"))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// What replaying one verdict found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Regraded {
    Same,
    /// Today's line differs. `to_pass` is `Some(true)` when more units pass than before,
    /// `Some(false)` when fewer, `None` when the count held but the detail changed.
    Flip {
        before: String,
        after: String,
        to_pass: Option<bool>,
    },
    Skipped(&'static str),
}

/// Compare a recorded verdict line with today's.
pub fn compare(before: &str, after: &str) -> Regraded {
    if before == after {
        return Regraded::Same;
    }
    let to_pass = match (pass_of(before), pass_of(after)) {
        (Some(b), Some(a)) if a > b => Some(true),
        (Some(b), Some(a)) if a < b => Some(false),
        _ => None,
    };
    Regraded::Flip {
        before: before.to_string(),
        after: after.to_string(),
        to_pass,
    }
}

/// Why a haystack could not be recovered.
const HAYSTACK_CHANGED: &str = "haystack changed";
const HAYSTACK_MISSING: &str = "haystack not recorded";
const IDEA_UNREADABLE: &str = "idea unreadable";
const NOT_A_VERDICT: &str = "not a verdict and its call";

/// The idea body and conversation prefix `r` names, read from `slug`'s live files, or why not.
/// The conversation is append-only, so its recorded-length prefix is compared by hash; the body
/// comes from the reference itself when the run rewrote it, else from the live `idea.md`.
fn recover_haystack(
    vault: &Path,
    slug: &str,
    r: &HaystackRef,
) -> Result<(String, String), &'static str> {
    let conversation = store::read_conversation(vault, slug).map_err(|_| IDEA_UNREADABLE)?;
    let len = usize::try_from(r.conversation_len).map_err(|_| HAYSTACK_CHANGED)?;
    let prefix = conversation.as_bytes().get(..len).ok_or(HAYSTACK_CHANGED)?;
    if sha256_hex(prefix) != r.conversation_sha256 {
        return Err(HAYSTACK_CHANGED);
    }
    let prefix = String::from_utf8(prefix.to_vec()).map_err(|_| HAYSTACK_CHANGED)?;
    let body = match &r.idea_body {
        Some(body) => body.clone(),
        None => {
            store::read_idea(vault, slug)
                .map_err(|_| IDEA_UNREADABLE)?
                .body
        }
    };
    if sha256_hex(body.as_bytes()) != r.idea_body_sha256 {
        return Err(HAYSTACK_CHANGED);
    }
    Ok((body, prefix))
}

/// Replay verdict `v` over the text of `call` with today's parser. Reads the idea's files only
/// when the parser needs its haystack; never writes.
pub fn regrade_entry(vault: &Path, slug: &str, v: &JournalEntry, call: &JournalEntry) -> Regraded {
    let (
        JournalEntry::Verdict {
            parser,
            summary,
            haystack,
            ..
        },
        JournalEntry::LlmCall { response_text, .. },
    ) = (v, call)
    else {
        return Regraded::Skipped(NOT_A_VERDICT);
    };
    let texts = match (parser.needs_haystack(), haystack) {
        (false, _) => None,
        (true, None) => return Regraded::Skipped(HAYSTACK_MISSING),
        (true, Some(r)) => match recover_haystack(vault, slug, r) {
            Ok(texts) => Some(texts),
            Err(why) => return Regraded::Skipped(why),
        },
    };
    let hay = texts.as_ref().map(|(idea_body, conversation)| Haystack {
        idea_body,
        conversation,
    });
    compare(summary, &summarize(parser, response_text, hay))
}

/// Which verdicts a run covers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    pub idea: Option<String>,
    /// A [`ParserKind::family`] spelling.
    pub parser: Option<String>,
}

/// The totals of one regrade run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegradeStats {
    pub same: u32,
    pub to_pass: u32,
    pub to_fail: u32,
    /// Flips whose pass count held.
    pub changed: u32,
    pub skipped: u32,
}

impl RegradeStats {
    pub fn flips(&self) -> u32 {
        self.to_pass + self.to_fail + self.changed
    }
}

/// Every run journal in the vault, as (idea slug, run id, path), sorted.
fn journals(vault: &Path, idea: Option<&str>) -> io::Result<Vec<(String, String, PathBuf)>> {
    let mut slugs: Vec<String> = match idea {
        Some(one) => vec![one.to_string()],
        None => std::fs::read_dir(vault)?
            .filter_map(Result::ok)
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect(),
    };
    slugs.retain(|s| slug::is_valid(s));
    slugs.sort();
    let mut out = Vec::new();
    for s in slugs {
        let Ok(entries) = std::fs::read_dir(journal::runs_dir(vault, &s)) else {
            continue;
        };
        let mut runs: Vec<(String, String, PathBuf)> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .filter_map(|p| {
                let id = p.file_stem()?.to_str()?.to_string();
                Some((s.clone(), id, p))
            })
            .collect();
        runs.sort();
        out.extend(runs);
    }
    Ok(out)
}

/// Replay every journaled verdict under `vault` that `filter` selects, print one line per flip
/// (and per unreadable journal) to `out`, then the totals. Read-only.
pub fn run(vault: &Path, filter: &Filter, out: &mut impl Write) -> io::Result<RegradeStats> {
    let mut stats = RegradeStats::default();
    let mut reasons: BTreeMap<&'static str, u32> = BTreeMap::new();
    for (idea, run_id, path) in journals(vault, filter.idea.as_deref())? {
        let entries = match journal::read_run(&path) {
            Ok(entries) => entries,
            Err(e) => {
                writeln!(out, "{idea}/{run_id} unreadable: {e}")?;
                continue;
            }
        };
        for v in &entries {
            let JournalEntry::Verdict {
                call_seq, parser, ..
            } = v
            else {
                continue;
            };
            if filter
                .parser
                .as_deref()
                .is_some_and(|p| p != parser.family())
            {
                continue;
            }
            let call = entries
                .iter()
                .find(|e| matches!(e, JournalEntry::LlmCall { seq, .. } if seq == call_seq));
            let regraded = match call {
                Some(call) => regrade_entry(vault, &idea, v, call),
                None => Regraded::Skipped("call not in journal"),
            };
            match regraded {
                Regraded::Same => stats.same += 1,
                Regraded::Skipped(why) => {
                    stats.skipped += 1;
                    *reasons.entry(why).or_default() += 1;
                }
                Regraded::Flip {
                    before,
                    after,
                    to_pass,
                } => {
                    match to_pass {
                        Some(true) => stats.to_pass += 1,
                        Some(false) => stats.to_fail += 1,
                        None => stats.changed += 1,
                    }
                    writeln!(
                        out,
                        "{idea}/{run_id} #{call_seq} {} {}",
                        parser.family(),
                        flip_detail(&before, &after)
                    )?;
                }
            }
        }
    }
    let why = if reasons.is_empty() {
        String::new()
    } else {
        format!(
            " ({})",
            reasons
                .iter()
                .map(|(r, n)| format!("{r}: {n}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    writeln!(
        out,
        "regrade: {} flip(s) to pass, {} to fail, {} changed, {} unchanged, {} skipped{why}",
        stats.to_pass, stats.to_fail, stats.changed, stats.same, stats.skipped
    )?;
    Ok(stats)
}

// ---- the committed corpus ----

/// One curated raw output: the parser that judges it, the text, and (for grounding parsers) the
/// idea body and conversation it was judged against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fixture {
    pub kind: ParserKind,
    /// `<slug>/<run_id>#<seq>` when exported from a journal; absent for a hand-seeded case.
    pub source: Option<String>,
    pub raw: String,
    pub idea: Option<String>,
    pub conversation: Option<String>,
}

const RAW_MARK: &str = "<!-- regrade:raw -->";
const IDEA_MARK: &str = "<!-- regrade:idea -->";
const CONVERSATION_MARK: &str = "<!-- regrade:conversation -->";

impl Fixture {
    /// The haystack this fixture carries, when it has both halves.
    pub fn haystack(&self) -> Option<Haystack<'_>> {
        Some(Haystack {
            idea_body: self.idea.as_deref()?,
            conversation: self.conversation.as_deref()?,
        })
    }

    /// Today's verdict line for this fixture.
    pub fn summary(&self) -> String {
        summarize(&self.kind, &self.raw, self.haystack())
    }

    /// Parse a fixture file: a `---` header of `key: value` lines (`parser`, plus `n` for an
    /// audit, `contract` for a contract, optional `source`), then the marked sections.
    pub fn parse(text: &str) -> Result<Fixture, String> {
        let rest = text
            .strip_prefix("---\n")
            .ok_or("fixture does not open with a --- header")?;
        let (header, body) = rest
            .split_once("\n---\n")
            .ok_or("fixture header is not closed by ---")?;
        let mut fields: BTreeMap<&str, &str> = BTreeMap::new();
        for line in header.lines() {
            let (k, v) = line
                .split_once(':')
                .ok_or_else(|| format!("header line is not `key: value`: {line}"))?;
            fields.insert(k.trim(), v.trim());
        }
        let field = |k: &str| {
            fields
                .get(k)
                .copied()
                .ok_or_else(|| format!("header lacks `{k}`"))
        };
        let kind = match field("parser")? {
            "audit" => ParserKind::Audit {
                n: field("n")?
                    .parse()
                    .map_err(|_| "header `n` is not a number".to_string())?,
            },
            "facts" => ParserKind::Facts,
            "contract" => {
                let name = field("contract")?;
                contract_named(name).ok_or_else(|| format!("unknown contract `{name}`"))?;
                ParserKind::Contract {
                    name: name.to_string(),
                }
            }
            "plan-gates" => ParserKind::PlanGates,
            other => return Err(format!("unknown parser `{other}`")),
        };
        let mut sections: BTreeMap<&str, String> = BTreeMap::new();
        let mut current: Option<&str> = None;
        for line in body.split_inclusive('\n') {
            let mark = line.trim_end_matches('\n');
            if [RAW_MARK, IDEA_MARK, CONVERSATION_MARK].contains(&mark) {
                if sections.contains_key(mark) {
                    return Err(format!("section {mark} appears twice"));
                }
                sections.insert(mark, String::new());
                current = Some(mark);
                continue;
            }
            match current {
                Some(c) => sections.entry(c).or_default().push_str(line),
                None if line.trim().is_empty() => {}
                None => return Err("text before the first section marker".into()),
            }
        }
        // The writer ends every section with one newline of its own.
        let mut take = |mark: &str| {
            sections.remove(mark).map(|mut s| {
                if s.ends_with('\n') {
                    s.pop();
                }
                s
            })
        };
        let raw = take(RAW_MARK).ok_or(format!("fixture lacks {RAW_MARK}"))?;
        let (idea, conversation) = (take(IDEA_MARK), take(CONVERSATION_MARK));
        if kind.needs_haystack() != (idea.is_some() && conversation.is_some()) {
            return Err(format!(
                "a {} fixture {} an idea and a conversation section",
                kind.family(),
                if kind.needs_haystack() {
                    "needs"
                } else {
                    "takes no"
                }
            ));
        }
        Ok(Fixture {
            source: fields.get("source").map(|s| s.to_string()),
            kind,
            raw,
            idea,
            conversation,
        })
    }

    /// The file text [`Self::parse`] reads back.
    pub fn render(&self) -> String {
        let mut out = format!("---\nparser: {}\n", self.kind.family());
        match &self.kind {
            ParserKind::Audit { n } => out.push_str(&format!("n: {n}\n")),
            ParserKind::Contract { name } => out.push_str(&format!("contract: {name}\n")),
            ParserKind::Facts | ParserKind::PlanGates => {}
        }
        if let Some(source) = &self.source {
            out.push_str(&format!("source: {source}\n"));
        }
        out.push_str("---\n");
        for (mark, text) in [
            (RAW_MARK, Some(&self.raw)),
            (IDEA_MARK, self.idea.as_ref()),
            (CONVERSATION_MARK, self.conversation.as_ref()),
        ] {
            if let Some(text) = text {
                out.push_str(&format!("{mark}\n{text}\n"));
            }
        }
        out
    }
}

/// Every fixture under `dir` (`<family>/<case>.md`), as (`<family>/<case>`, parse result), sorted.
pub fn read_corpus(dir: &Path) -> io::Result<Vec<(String, Result<Fixture, String>)>> {
    let mut out = Vec::new();
    for family in std::fs::read_dir(dir)? {
        let family = family?.path();
        if !family.is_dir() {
            continue;
        }
        let fam = family
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or_default()
            .to_string();
        for case in std::fs::read_dir(&family)? {
            let case = case?.path();
            if case.extension().is_none_or(|x| x != "md") {
                continue;
            }
            let name = case
                .file_stem()
                .and_then(|f| f.to_str())
                .unwrap_or_default();
            let parsed = std::fs::read_to_string(&case)
                .map_err(|e| e.to_string())
                .and_then(|text| Fixture::parse(&text))
                .and_then(|f| {
                    if f.kind.family() == fam {
                        Ok(f)
                    } else {
                        Err(format!("a {} fixture filed under {fam}/", f.kind.family()))
                    }
                });
            out.push((format!("{fam}/{name}"), parsed));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// The corpus snapshot: one `<family>/<case> <verdict line>` per fixture, sorted. A fixture that
/// does not parse is an error, never a line.
pub fn corpus_snapshot(dir: &Path) -> Result<String, String> {
    let mut out = String::new();
    for (name, fixture) in read_corpus(dir).map_err(|e| e.to_string())? {
        let fixture = fixture.map_err(|e| format!("{name}: {e}"))?;
        out.push_str(&format!("{name} {}\n", fixture.summary()));
    }
    Ok(out)
}

/// The flip report between two corpus snapshots: one line per case whose verdict changed, plus
/// cases only one side has.
pub fn snapshot_flips(committed: &str, today: &str) -> Vec<String> {
    let parse = |s: &str| -> BTreeMap<String, String> {
        s.lines()
            .filter_map(|l| l.split_once(' '))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    let (before, after) = (parse(committed), parse(today));
    let mut names: Vec<&String> = before.keys().chain(after.keys()).collect();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .filter_map(|name| match (before.get(name), after.get(name)) {
            (Some(b), Some(a)) if a == b => None,
            (Some(b), Some(a)) => Some(format!("{name} {}", flip_detail(b, a))),
            (Some(_), None) => Some(format!("{name} removed from the corpus")),
            (None, _) => Some(format!("{name} new, not in the snapshot")),
        })
        .collect()
}

/// Why an export could not be made.
#[derive(Debug, thiserror::Error)]
pub enum RegradeError {
    #[error("the reference must be <slug>/<run_id>#<seq>: {0}")]
    BadRef(String),
    #[error("the case name must be a slug: {0}")]
    BadCase(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}: the haystack this verdict was judged against is no longer on disk")]
    Haystack(&'static str),
    #[error("{0} already exists; pick another case name")]
    Exists(PathBuf),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Copy the answer of call `<slug>/<run_id>#<seq>`, its parser and its haystack into
/// `<corpus>/<family>/<case>.md`. The only write regrade makes, and never into the vault.
pub fn export(
    vault: &Path,
    reference: &str,
    case: &str,
    corpus: &Path,
) -> Result<PathBuf, RegradeError> {
    let bad = || RegradeError::BadRef(reference.to_string());
    let (idea, rest) = reference.split_once('/').ok_or_else(bad)?;
    let (run_id, seq) = rest.split_once('#').ok_or_else(bad)?;
    let seq: u32 = seq.parse().map_err(|_| bad())?;
    if !slug::is_valid(idea) || run_id.is_empty() || run_id.contains(['/', '\\', '.']) {
        return Err(bad());
    }
    if !slug::is_valid(case) {
        return Err(RegradeError::BadCase(case.to_string()));
    }
    let path = journal::runs_dir(vault, idea).join(format!("{run_id}.jsonl"));
    let entries = journal::read_run(&path)
        .map_err(|e| RegradeError::NotFound(format!("{}: {e}", path.display())))?;
    let raw = entries
        .iter()
        .find_map(|e| match e {
            JournalEntry::LlmCall {
                seq: s,
                response_text,
                ..
            } if *s == seq => Some(response_text.clone()),
            _ => None,
        })
        .ok_or_else(|| RegradeError::NotFound(format!("no call #{seq} in {run_id}")))?;
    let (kind, haystack) = entries
        .iter()
        .find_map(|e| match e {
            JournalEntry::Verdict {
                call_seq,
                parser,
                haystack,
                ..
            } if *call_seq == seq => Some((parser.clone(), haystack.clone())),
            _ => None,
        })
        .ok_or_else(|| RegradeError::NotFound(format!("call #{seq} has no verdict to replay")))?;
    let (idea_text, conversation) = match (kind.needs_haystack(), haystack) {
        (false, _) => (None, None),
        (true, None) => return Err(RegradeError::Haystack(HAYSTACK_MISSING)),
        (true, Some(r)) => {
            let (body, conv) = recover_haystack(vault, idea, &r).map_err(RegradeError::Haystack)?;
            (Some(body), Some(conv))
        }
    };
    let fixture = Fixture {
        source: Some(reference.to_string()),
        kind,
        raw,
        idea: idea_text,
        conversation,
    };
    let dir = corpus.join(fixture.kind.family());
    std::fs::create_dir_all(&dir)?;
    let target = dir.join(format!("{case}.md"));
    // create_new: an export never overwrites a curated case.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target)
        .map_err(|e| match e.kind() {
            io::ErrorKind::AlreadyExists => RegradeError::Exists(target.clone()),
            _ => RegradeError::Io(e),
        })?;
    file.write_all(fixture.render().as_bytes())?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::call::CallMeta;
    use crate::ai::journal::{JournalWriter, RunKind, FORMAT_VERSION};
    use crate::domain::{Idea, IdeaFrontmatter, IdeaState};

    const RUN: &str = "20260930T120000000Z-swarm";
    const AUDIT: &str = "F1: CONFIRMED — follows\nF2: REFUTED — answered\nF3: UNCERTAIN — open";
    const FACTS: &str = "FACT: Ship solo\nOP: ADD\nQUOTE: \"we ship v1 solo\"\nNo hires.\n\
FACT: Invented\nQUOTE: \"never said\"\nMade up.\nTAGS: solo, launch";

    fn idea(vault: &Path, body: &str, conversation: &str) {
        let now = chrono::Utc::now();
        store::create_idea(
            vault,
            &Idea {
                frontmatter: IdeaFrontmatter {
                    title: "Idea".into(),
                    slug: "idea".into(),
                    state: IdeaState::InDiscussion,
                    tags: Vec::new(),
                    sources: Vec::new(),
                    created: now,
                    updated: now,
                    extra: Default::default(),
                },
                body: body.into(),
            },
        )
        .unwrap();
        std::fs::write(vault.join("idea/conversation.md"), conversation).unwrap();
    }

    /// A journal holding call #1 answering `raw` and a verdict on it recorded as `summary`.
    fn journal(
        vault: &Path,
        raw: &str,
        parser: ParserKind,
        summary: String,
        hay: Option<HaystackRef>,
    ) {
        let dir = journal::runs_dir(vault, "idea");
        std::fs::create_dir_all(&dir).unwrap();
        let mut w = JournalWriter::create(
            &dir.join(format!("{RUN}.jsonl")),
            JournalEntry::RunStarted {
                format_version: FORMAT_VERSION,
                run_id: RUN.into(),
                slug: "idea".into(),
                kind: RunKind::Swarm,
                build: "test".into(),
                ts_ms: 0,
            },
        )
        .unwrap();
        for e in [
            JournalEntry::LlmCall {
                seq: 1,
                role: Some("auditor".into()),
                backend: "ollama".into(),
                model: "m".into(),
                temperature_milli: None,
                request_sha256: String::new(),
                response_text: raw.into(),
                meta: CallMeta::default(),
            },
            JournalEntry::Verdict {
                call_seq: 1,
                parser,
                summary,
                haystack: hay,
            },
        ] {
            w.append(&e).unwrap();
        }
        w.finish(journal::RunOutcome::Done).unwrap();
    }

    fn run_all(vault: &Path) -> (RegradeStats, String) {
        let mut out = Vec::new();
        let stats = run(vault, &Filter::default(), &mut out).unwrap();
        (stats, String::from_utf8(out).unwrap())
    }

    fn facts_verdict(vault: &Path) {
        let (body, conv) = ("An idea.\n", "## user\nWe ship v1 solo.\n\n");
        idea(vault, body, conv);
        let hay = Haystack {
            idea_body: body,
            conversation: conv,
        };
        let summary = summarize(&ParserKind::Facts, FACTS, Some(hay));
        assert!(summary.starts_with("pass=1 "), "{summary}");
        journal(
            vault,
            FACTS,
            ParserKind::Facts,
            summary,
            Some(HaystackRef::of(hay)),
        );
    }

    #[test]
    fn unchanged_parser_yields_zero_flips() {
        let tmp = tempfile::tempdir().unwrap();
        idea(tmp.path(), "body\n", "");
        let kind = ParserKind::Audit { n: 3 };
        journal(
            tmp.path(),
            AUDIT,
            kind.clone(),
            summarize(&kind, AUDIT, None),
            None,
        );
        let (stats, out) = run_all(tmp.path());
        assert_eq!(stats.flips(), 0, "{out}");
        assert_eq!(stats.same, 1);
        assert!(out.starts_with("regrade: 0 flip(s)"), "{out}");
    }

    #[test]
    fn changed_audit_parser_reports_flip_line() {
        let tmp = tempfile::tempdir().unwrap();
        idea(tmp.path(), "body\n", "");
        // What an older parser recorded: it read F3 as CONFIRMED and missed F2.
        let recorded = "pass=2 n=3 failed=false F1=CONFIRMED F2=unanswered F3=CONFIRMED";
        journal(
            tmp.path(),
            AUDIT,
            ParserKind::Audit { n: 3 },
            recorded.into(),
            None,
        );
        let (stats, out) = run_all(tmp.path());
        assert_eq!(stats.to_pass, 1, "{out}");
        assert!(
            out.contains(&format!(
                "idea/{RUN} #1 audit F2 unanswered->REFUTED, F3 CONFIRMED->UNCERTAIN\n"
            )),
            "{out}"
        );
    }

    #[test]
    fn edited_idea_body_skips_facts_verdict() {
        let tmp = tempfile::tempdir().unwrap();
        facts_verdict(tmp.path());
        assert_eq!(run_all(tmp.path()).0.same, 1);
        let mut i = store::read_idea(tmp.path(), "idea").unwrap();
        i.body = "An edited idea.\n".into();
        store::write_idea(tmp.path(), &i).unwrap();
        let (stats, out) = run_all(tmp.path());
        assert_eq!(
            (stats.skipped, stats.same, stats.flips()),
            (1, 0, 0),
            "{out}"
        );
        assert!(out.contains("haystack changed: 1"), "{out}");
    }

    #[test]
    fn appended_conversation_still_regrades_via_prefix_hash() {
        let tmp = tempfile::tempdir().unwrap();
        facts_verdict(tmp.path());
        store::append_turn(tmp.path(), "idea", "user", "never said, now said").unwrap();
        let (stats, out) = run_all(tmp.path());
        assert_eq!((stats.same, stats.skipped), (1, 0), "{out}");
    }

    #[test]
    fn rewritten_body_regrades_from_the_recorded_text() {
        let tmp = tempfile::tempdir().unwrap();
        let (body, conv) = ("Pre-store body.\n", "## user\nWe ship v1 solo.\n\n");
        idea(tmp.path(), "Consolidated after the store.\n", conv);
        let hay = Haystack {
            idea_body: body,
            conversation: conv,
        };
        let summary = summarize(&ParserKind::Facts, FACTS, Some(hay));
        let r = HaystackRef::of_rewritten(hay);
        journal(tmp.path(), FACTS, ParserKind::Facts, summary, Some(r));
        let (stats, out) = run_all(tmp.path());
        assert_eq!((stats.same, stats.skipped), (1, 0), "{out}");
    }

    fn tree_digest(dir: &Path) -> String {
        let mut entries: Vec<String> = walkdir::WalkDir::new(dir)
            .into_iter()
            .map(|e| e.unwrap())
            .map(|e| {
                let bytes = if e.file_type().is_file() {
                    std::fs::read(e.path()).unwrap()
                } else {
                    Vec::new()
                };
                format!("{} {}", e.path().display(), sha256_hex(&bytes))
            })
            .collect();
        entries.sort();
        sha256_hex(entries.join("\n").as_bytes())
    }

    #[test]
    fn never_writes_to_vault() {
        let tmp = tempfile::tempdir().unwrap();
        facts_verdict(tmp.path());
        let before = tree_digest(tmp.path());
        run_all(tmp.path());
        let mut out = Vec::new();
        run(
            tmp.path(),
            &Filter {
                idea: Some("idea".into()),
                parser: Some("facts".into()),
            },
            &mut out,
        )
        .unwrap();
        assert_eq!(tree_digest(tmp.path()), before);
    }

    #[test]
    fn parser_filter_leaves_other_verdicts_out() {
        let tmp = tempfile::tempdir().unwrap();
        facts_verdict(tmp.path());
        let mut out = Vec::new();
        let stats = run(
            tmp.path(),
            &Filter {
                idea: None,
                parser: Some("audit".into()),
            },
            &mut out,
        )
        .unwrap();
        assert_eq!(stats, RegradeStats::default());
    }

    #[test]
    fn export_writes_a_fixture_that_parses_back_and_refuses_to_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let corpus = tempfile::tempdir().unwrap();
        facts_verdict(tmp.path());
        let reference = format!("idea/{RUN}#1");
        let path = export(tmp.path(), &reference, "solo-case", corpus.path()).unwrap();
        assert_eq!(path, corpus.path().join("facts/solo-case.md"));
        let fixture = Fixture::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(fixture.raw, FACTS);
        assert_eq!(fixture.source.as_deref(), Some(reference.as_str()));
        assert_eq!(
            fixture.conversation.as_deref(),
            Some("## user\nWe ship v1 solo.\n\n")
        );
        assert!(fixture.summary().starts_with("pass=1 "));
        assert!(matches!(
            export(tmp.path(), &reference, "solo-case", corpus.path()),
            Err(RegradeError::Exists(_))
        ));
        assert!(matches!(
            export(tmp.path(), "idea/../x#1", "c", corpus.path()),
            Err(RegradeError::BadRef(_))
        ));
    }

    #[test]
    fn fixture_render_round_trips_exact_text() {
        let f = Fixture {
            kind: ParserKind::Contract {
                name: "ranked_list".into(),
            },
            source: None,
            raw: "\n1. first\n<!-- not a marker -->\n\n".into(),
            idea: None,
            conversation: None,
        };
        assert_eq!(Fixture::parse(&f.render()).unwrap(), f);
    }

    #[test]
    fn flip_detail_names_only_changed_tokens() {
        assert_eq!(
            flip_detail("pass=1 a=x b=y", "pass=2 a=x b=z c=w"),
            "b y->z, c ∅->w"
        );
        assert_eq!(compare("pass=1 a=x", "pass=1 a=x"), Regraded::Same);
    }
}
