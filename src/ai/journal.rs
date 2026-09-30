//! The run journal (docs/adr/0037, D39): one append-only JSONL file per AI job at
//! `vault/<slug>/.runs/<run_id>.jsonl`, holding every model call's verbatim response, its
//! [`CallMeta`], the Ollama tool rounds and each contract outcome.
//!
//! The journal is diagnostics, never truth: reindex skips the dot-dir, no prompt ever reads it,
//! fork does not copy it, and only the newest [`KEEP_RUNS`] runs per idea are kept. A journal that
//! cannot be opened or written costs one warning and the job runs unjournaled; it never fails a
//! turn. It records calls for inspection only and is unrelated to ADR-0033's MCP result
//! idempotency.
//!
//! Every field is an integer or a string: a float would serialize differently across platforms,
//! so the temperature is stored in thousandths.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::ai::call::CallMeta;
use crate::ai::contract::ContractOutcome;
use crate::ai::verdict::{HaystackRef, ParserKind};

/// The journal line format; bumped on any incompatible entry change.
pub const FORMAT_VERSION: u32 = 1;

/// How many runs each idea keeps (owner decision, 2026-09-30): the oldest go when a new one opens.
pub const KEEP_RUNS: usize = 50;

/// The journal directory inside an idea folder. A dot-dir, so reindex and the vault walkers pass
/// over it (ADR-0002: it is not truth).
pub const RUNS_DIR: &str = ".runs";

/// Tool results are capped like the fetch tool's own output, so one noisy source cannot balloon a
/// journal.
pub const TOOL_RESULT_CAP: usize = 12_000;

/// What kind of job a run is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    Chat,
    Skill,
    Swarm,
    Workflow,
    Extract,
    Compact,
    Store,
    BuildPlan,
    Replan,
}

impl RunKind {
    /// The spelling in a run id.
    pub fn as_str(self) -> &'static str {
        match self {
            RunKind::Chat => "chat",
            RunKind::Skill => "skill",
            RunKind::Swarm => "swarm",
            RunKind::Workflow => "workflow",
            RunKind::Extract => "extract",
            RunKind::Compact => "compact",
            RunKind::Store => "store",
            RunKind::BuildPlan => "build-plan",
            RunKind::Replan => "replan",
        }
    }
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RunOutcome {
    Done,
    Failed {
        message: String,
    },
    /// The job was cancelled, or dropped without reporting an outcome.
    Cancelled,
    Panicked,
}

/// One journal line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JournalEntry {
    RunStarted {
        format_version: u32,
        run_id: String,
        slug: String,
        kind: RunKind,
        build: String,
        ts_ms: i64,
    },
    /// One completed model call, with the verbatim text it returned.
    LlmCall {
        seq: u32,
        role: Option<String>,
        backend: String,
        model: String,
        temperature_milli: Option<u32>,
        request_sha256: String,
        response_text: String,
        meta: CallMeta,
    },
    /// One executed Ollama tool call inside call `call_seq`.
    ToolCall {
        call_seq: u32,
        round: u32,
        name: String,
        args_sha256: String,
        result_text: String,
        is_error: bool,
    },
    /// A deterministic parser's verdict on the answer of call `call_seq`, as
    /// `regrade::summarize` wrote it (docs/adr/0038): what `regrade` replays today's parser
    /// against. `haystack` names the evidence a grounding parser read.
    Verdict {
        call_seq: u32,
        parser: ParserKind,
        summary: String,
        haystack: Option<HaystackRef>,
    },
    /// How the answer of call `call_seq` met its output contract (docs/adr/0023).
    Contract {
        call_seq: u32,
        contract: String,
        outcome: ContractOutcome,
    },
    RunFinished {
        #[serde(flatten)]
        outcome: RunOutcome,
        llm_calls: u32,
        ts_ms: i64,
    },
}

/// The open journal of one run. Appends are flushed line by line, so a crash leaves a parseable
/// prefix; dropping it unfinished records the run as cancelled (or panicked, mid-unwind).
pub struct JournalWriter {
    out: BufWriter<File>,
    run_id: String,
    finished: bool,
    /// Set after the first write error: the run goes on unjournaled with one warning.
    broken: bool,
    next_seq: u32,
    llm_calls: u32,
    /// The call each contract last settled on, by contract name: how a verdict computed after
    /// the call returned (a build plan's gates run once the planner's answer is kept) finds the
    /// call it judges.
    last_contract_call: std::collections::BTreeMap<String, u32>,
}

/// A run's journal as the job and every scoped backend clone share it.
pub type JournalHandle = Arc<Mutex<JournalWriter>>;

impl JournalWriter {
    /// Create the journal at `path` with its `RunStarted` line. Refuses an existing file, so two
    /// runs can never interleave in one journal.
    pub fn create(path: &Path, started: JournalEntry) -> io::Result<Self> {
        let run_id = match &started {
            JournalEntry::RunStarted { run_id, .. } => run_id.clone(),
            _ => String::new(),
        };
        let file = OpenOptions::new().write(true).create_new(true).open(path)?;
        let mut writer = JournalWriter {
            out: BufWriter::new(file),
            run_id,
            finished: false,
            broken: false,
            next_seq: 1,
            llm_calls: 0,
            last_contract_call: std::collections::BTreeMap::new(),
        };
        writer.append(&started)?;
        Ok(writer)
    }

    /// Write one entry as one line and flush it.
    pub fn append(&mut self, e: &JournalEntry) -> io::Result<()> {
        let line = serde_json::to_string(e).map_err(io::Error::other)?;
        self.out.write_all(line.as_bytes())?;
        self.out.write_all(b"\n")?;
        self.out.flush()
    }

    /// Append, and on the first failure warn once and stop writing: a journal never fails a turn.
    fn append_logged(&mut self, e: &JournalEntry) {
        if self.broken {
            return;
        }
        if let Err(err) = self.append(e) {
            self.broken = true;
            tracing::warn!(run = %self.run_id, error = %err, "run journal write failed; continuing unjournaled");
        }
    }

    /// Write the `RunFinished` line once; later calls are no-ops.
    pub fn finish(&mut self, o: RunOutcome) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        self.append(&JournalEntry::RunFinished {
            outcome: o,
            llm_calls: self.llm_calls,
            ts_ms: chrono::Utc::now().timestamp_millis(),
        })
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// The seq the next recorded call will get.
    pub fn next_seq(&self) -> u32 {
        self.next_seq
    }

    /// Record one call: its `LlmCall` line under a fresh seq, then its tool rounds. Returns the
    /// seq, for the call's later `Contract` line.
    pub fn record_call(&mut self, call: CallRecord, tools: Vec<ToolRecord>) -> u32 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.llm_calls += 1;
        self.append_logged(&JournalEntry::LlmCall {
            seq,
            role: call.role,
            backend: call.backend,
            model: call.model,
            temperature_milli: call.temperature_milli,
            request_sha256: call.request_sha256,
            response_text: call.response_text,
            meta: call.meta,
        });
        for t in tools {
            self.append_logged(&JournalEntry::ToolCall {
                call_seq: seq,
                round: t.round,
                name: t.name,
                args_sha256: t.args_sha256,
                result_text: t.result_text,
                is_error: t.is_error,
            });
        }
        seq
    }

    /// Record how call `call_seq`'s answer met `contract`.
    pub fn record_contract(&mut self, call_seq: u32, contract: &str, outcome: ContractOutcome) {
        self.last_contract_call
            .insert(contract.to_string(), call_seq);
        self.append_logged(&JournalEntry::Contract {
            call_seq,
            contract: contract.to_string(),
            outcome,
        });
    }

    /// Record a parser's verdict on the answer of call `call_seq` (docs/adr/0038).
    pub fn record_verdict(
        &mut self,
        call_seq: u32,
        parser: ParserKind,
        summary: String,
        haystack: Option<HaystackRef>,
    ) {
        self.append_logged(&JournalEntry::Verdict {
            call_seq,
            parser,
            summary,
            haystack,
        });
    }

    /// The call `contract` (its frontmatter name) last settled on in this run, if any.
    pub fn last_contract_call(&self, contract: &str) -> Option<u32> {
        self.last_contract_call.get(contract).copied()
    }
}

impl Drop for JournalWriter {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let outcome = if std::thread::panicking() {
            RunOutcome::Panicked
        } else {
            RunOutcome::Cancelled
        };
        // Best-effort: the run is already over, and a failed write here only loses the ending.
        let _ = self.finish(outcome);
    }
}

/// One completed call, as the backend hands it to [`JournalWriter::record_call`].
pub struct CallRecord {
    pub role: Option<String>,
    pub backend: String,
    pub model: String,
    pub temperature_milli: Option<u32>,
    pub request_sha256: String,
    pub response_text: String,
    pub meta: CallMeta,
}

/// One executed tool call of an Ollama tool loop.
pub struct ToolRecord {
    pub round: u32,
    pub name: String,
    pub args_sha256: String,
    pub result_text: String,
    pub is_error: bool,
}

/// SHA-256 of `text`, hex.
pub fn sha256_hex(text: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `text` cut to at most [`TOOL_RESULT_CAP`] chars.
pub fn cap_tool_result(text: &str) -> String {
    text.chars().take(TOOL_RESULT_CAP).collect()
}

/// This binary's build: the crate version, plus the commit when the build injected one.
fn build_id() -> String {
    match option_env!("IDEA_VAULT_BUILD_SHA") {
        Some(sha) if !sha.is_empty() => format!("{}+{sha}", env!("CARGO_PKG_VERSION")),
        _ => env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// A run id: UTC time to the millisecond, then the kind, so ids sort by start time.
fn mint_run_id(kind: RunKind) -> String {
    format!(
        "{}-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S%3fZ"),
        kind.as_str()
    )
}

/// The journal directory of idea `slug`.
pub fn runs_dir(vault: &Path, slug: &str) -> PathBuf {
    vault.join(slug).join(RUNS_DIR)
}

/// Open a new run for idea `slug`: mint its id, create the journal with its `RunStarted` line and
/// prune the idea's runs to the newest [`KEEP_RUNS`]. `None` (with one warning) when the slug is
/// invalid or the file cannot be created; the job then runs unjournaled.
pub fn open_run(vault: &Path, slug: &str, kind: RunKind) -> Option<JournalHandle> {
    if !crate::domain::slug::is_valid(slug) {
        tracing::warn!(
            slug,
            "run journal refused an invalid slug; running unjournaled"
        );
        return None;
    }
    let dir = runs_dir(vault, slug);
    let run_id = mint_run_id(kind);
    let started = JournalEntry::RunStarted {
        format_version: FORMAT_VERSION,
        run_id: run_id.clone(),
        slug: slug.to_string(),
        kind,
        build: build_id(),
        ts_ms: chrono::Utc::now().timestamp_millis(),
    };
    let opened = std::fs::create_dir_all(&dir)
        .and_then(|()| JournalWriter::create(&dir.join(format!("{run_id}.jsonl")), started));
    match opened {
        Ok(writer) => {
            prune(&dir, KEEP_RUNS);
            Some(Arc::new(Mutex::new(writer)))
        }
        Err(e) => {
            tracing::warn!(slug, error = %e, "run journal could not be opened; running unjournaled");
            None
        }
    }
}

/// Delete all but the newest `keep` journals in `dir`. Run ids sort by start time, so the names
/// order the files.
fn prune(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut runs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    runs.sort();
    let excess = runs.len().saturating_sub(keep);
    for old in &runs[..excess] {
        if let Err(e) = std::fs::remove_file(old) {
            tracing::warn!(path = %old.display(), error = %e, "could not prune an old run journal");
        }
    }
}

/// Record the run's outcome on `handle`, if there is one. A no-op after the first outcome.
pub fn finish(handle: Option<&JournalHandle>, outcome: RunOutcome) {
    let Some(handle) = handle else {
        return;
    };
    if let Ok(mut j) = handle.lock() {
        if let Err(e) = j.finish(outcome) {
            tracing::warn!(run = %j.run_id(), error = %e, "run journal could not record its end");
        }
    }
}

/// Read a journal back. A torn last line (the process died mid-write) is dropped; a bad line
/// anywhere else is an error, because the file was not written by [`JournalWriter`].
pub fn read_run(path: &Path) -> io::Result<Vec<JournalEntry>> {
    let lines: Vec<String> = BufReader::new(File::open(path)?)
        .lines()
        .collect::<io::Result<_>>()?;
    let mut entries = Vec::with_capacity(lines.len());
    for (i, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<JournalEntry>(line) {
            Ok(e) => entries.push(e),
            Err(_) if i + 1 == lines.len() => break,
            Err(e) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("journal line {}: {e}", i + 1),
                ))
            }
        }
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::call::CallUsage;

    fn started(run_id: &str) -> JournalEntry {
        JournalEntry::RunStarted {
            format_version: FORMAT_VERSION,
            run_id: run_id.into(),
            slug: "idea".into(),
            kind: RunKind::Skill,
            build: "0.0.0".into(),
            ts_ms: 1,
        }
    }

    fn call(text: &str) -> CallRecord {
        CallRecord {
            role: Some("critic".into()),
            backend: "ollama".into(),
            model: "llama3.2".into(),
            temperature_milli: Some(700),
            request_sha256: sha256_hex("req"),
            response_text: text.into(),
            meta: CallMeta {
                usage: CallUsage {
                    prompt_tokens: Some(10),
                    output_tokens: Some(5),
                    api_calls: 1,
                },
                stop_reason: Some("stop".into()),
                num_ctx: Some(8192),
                ms: 12,
                journal_seq: None,
            },
        }
    }

    #[test]
    fn create_new_refuses_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("r.jsonl");
        std::fs::write(&path, "already here\n").unwrap();
        let err = JournalWriter::create(&path, started("r")).err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "already here\n");
    }

    #[test]
    fn every_line_is_flushed_and_parseable_prefix_survives_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("r.jsonl");
        let mut w = JournalWriter::create(&path, started("r")).unwrap();
        let seq = w.record_call(call("first answer"), Vec::new());
        // Read while the writer is still open: every line is already on disk.
        let entries = read_run(&path).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(
            matches!(&entries[1], JournalEntry::LlmCall { seq: 1, response_text, .. } if response_text == "first answer")
        );
        w.record_contract(seq, "ranked_list", ContractOutcome::Repaired);
        std::mem::forget(w);
        let entries = read_run(&path).unwrap();
        assert_eq!(
            entries.len(),
            3,
            "a writer that never finished leaves its prefix"
        );
        assert!(matches!(
            entries[2],
            JournalEntry::Contract {
                call_seq: 1,
                outcome: ContractOutcome::Repaired,
                ..
            }
        ));
    }

    #[test]
    fn drop_without_finish_writes_cancelled() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("r.jsonl");
        {
            let mut w = JournalWriter::create(&path, started("r")).unwrap();
            w.record_call(call("x"), Vec::new());
        }
        let entries = read_run(&path).unwrap();
        assert_eq!(
            entries.last(),
            Some(&JournalEntry::RunFinished {
                outcome: RunOutcome::Cancelled,
                llm_calls: 1,
                ts_ms: match entries.last() {
                    Some(JournalEntry::RunFinished { ts_ms, .. }) => *ts_ms,
                    _ => 0,
                },
            })
        );
    }

    #[test]
    fn finish_is_written_once() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("r.jsonl");
        let mut w = JournalWriter::create(&path, started("r")).unwrap();
        w.finish(RunOutcome::Failed {
            message: "boom".into(),
        })
        .unwrap();
        w.finish(RunOutcome::Done).unwrap();
        drop(w);
        let finished: Vec<_> = read_run(&path)
            .unwrap()
            .into_iter()
            .filter(|e| matches!(e, JournalEntry::RunFinished { .. }))
            .collect();
        assert_eq!(finished.len(), 1);
        assert!(matches!(
            &finished[0],
            JournalEntry::RunFinished { outcome: RunOutcome::Failed { message }, .. } if message == "boom"
        ));
    }

    /// Walk a JSON value and fail on any float.
    fn assert_no_floats(v: &serde_json::Value, line: &str) {
        match v {
            serde_json::Value::Number(n) => assert!(!n.is_f64(), "float in {line}"),
            serde_json::Value::Array(a) => a.iter().for_each(|x| assert_no_floats(x, line)),
            serde_json::Value::Object(o) => o.values().for_each(|x| assert_no_floats(x, line)),
            _ => {}
        }
    }

    #[test]
    fn no_floats_in_serialized_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("r.jsonl");
        let mut w = JournalWriter::create(&path, started("r")).unwrap();
        let seq = w.record_call(
            call("answer"),
            vec![ToolRecord {
                round: 0,
                name: "source_read".into(),
                args_sha256: sha256_hex("{}"),
                result_text: "file".into(),
                is_error: false,
            }],
        );
        w.record_contract(seq, "free", ContractOutcome::OffContract("empty".into()));
        w.finish(RunOutcome::Done).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let mut kinds = Vec::new();
        for line in text.lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            kinds.push(v["type"].as_str().unwrap().to_string());
            assert_no_floats(&v, line);
        }
        assert_eq!(
            kinds,
            [
                "run_started",
                "llm_call",
                "tool_call",
                "contract",
                "run_finished"
            ]
        );
    }

    #[test]
    fn read_run_tolerates_torn_last_line() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("r.jsonl");
        let mut w = JournalWriter::create(&path, started("r")).unwrap();
        w.record_call(call("whole"), Vec::new());
        std::mem::forget(w);
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"type\":\"llm_call\",\"seq\":2,\"resp")
            .unwrap();
        assert_eq!(read_run(&path).unwrap().len(), 2);

        let torn_middle = tmp.path().join("m.jsonl");
        std::fs::write(
            &torn_middle,
            format!(
                "{}\n{{\"type\":\n{}\n",
                serde_json::to_string(&started("m")).unwrap(),
                serde_json::to_string(&started("m")).unwrap()
            ),
        )
        .unwrap();
        assert_eq!(
            read_run(&torn_middle).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn open_run_mints_a_sortable_id_and_keeps_the_newest_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = runs_dir(tmp.path(), "idea");
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..KEEP_RUNS {
            std::fs::write(dir.join(format!("20000101T0000{i:02}000Z-chat.jsonl")), "").unwrap();
        }
        let handle = open_run(tmp.path(), "idea", RunKind::Replan).unwrap();
        let run_id = handle.lock().unwrap().run_id().to_string();
        assert!(run_id.ends_with("Z-replan"), "{run_id}");
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left.len(), KEEP_RUNS);
        assert!(!left.contains(&"20000101T000000000Z-chat.jsonl".to_string()));
        assert_eq!(left.last().unwrap(), &format!("{run_id}.jsonl"));
    }

    #[test]
    fn open_run_refuses_an_invalid_slug() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(open_run(tmp.path(), "../escape", RunKind::Chat).is_none());
    }
}
