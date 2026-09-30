//! The `idea-vault regrade` subcommand end to end (docs/adr/0038, D40): the real binary over a
//! temp vault holding one run journal. Exit 0 with a flip report by default, 1 on a flip only
//! with `--strict`; `--export` writes a corpus fixture under the working directory and nothing
//! into the vault.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; HTC-6 binds shipping code"
)]

mod support;

use std::path::Path;
use std::process::{Command, Output};

use idea_vault::ai::call::CallMeta;
use idea_vault::ai::journal::{self, JournalEntry, JournalWriter, RunKind, RunOutcome};
use idea_vault::ai::verdict::ParserKind;

const RUN: &str = "20260930T120000000Z-swarm";
const AUDIT: &str = "F1: CONFIRMED — holds\nF2: UNCERTAIN — open";

/// A vault with idea `idea` whose journal records an audit verdict of `summary` on `AUDIT`.
fn vault_with_verdict(summary: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let dir = journal::runs_dir(tmp.path(), "idea");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(tmp.path().join("idea/conversation.md"), "").unwrap();
    let mut w = JournalWriter::create(
        &dir.join(format!("{RUN}.jsonl")),
        JournalEntry::RunStarted {
            format_version: journal::FORMAT_VERSION,
            run_id: RUN.into(),
            slug: "idea".into(),
            kind: RunKind::Swarm,
            build: "test".into(),
            ts_ms: 0,
        },
    )
    .unwrap();
    w.append(&JournalEntry::LlmCall {
        seq: 1,
        role: Some("auditor".into()),
        backend: "ollama".into(),
        model: "m".into(),
        temperature_milli: None,
        request_sha256: String::new(),
        response_text: AUDIT.into(),
        meta: CallMeta::default(),
    })
    .unwrap();
    w.append(&JournalEntry::Verdict {
        call_seq: 1,
        parser: ParserKind::Audit { n: 2 },
        summary: summary.into(),
        haystack: None,
    })
    .unwrap();
    w.finish(RunOutcome::Done).unwrap();
    tmp
}

fn regrade(vault: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_idea-vault"))
        .arg("regrade")
        .args(args)
        .current_dir(cwd)
        .env("IDEA_VAULT_VAULT_DIR", vault)
        .env("IDEA_VAULT_INDEX_PATH", cwd.join("index.db"))
        .output()
        .unwrap()
}

const TODAY: &str = "pass=2 n=2 failed=false F1=CONFIRMED F2=UNCERTAIN";
const OLDER: &str = "pass=1 n=2 failed=false F1=CONFIRMED F2=unanswered";

#[test]
fn strict_exits_1_on_flip() {
    let vault = vault_with_verdict(OLDER);
    let cwd = tempfile::tempdir().unwrap();
    let lax = regrade(vault.path(), cwd.path(), &[]);
    let stdout = String::from_utf8_lossy(&lax.stdout);
    assert_eq!(lax.status.code(), Some(0), "{stdout}");
    assert!(
        stdout.contains(&format!("idea/{RUN} #1 audit F2 unanswered->UNCERTAIN")),
        "{stdout}"
    );
    assert!(stdout.contains("regrade: 1 flip(s) to pass"), "{stdout}");
    let strict = regrade(vault.path(), cwd.path(), &["--strict"]);
    assert_eq!(strict.status.code(), Some(1));
}

#[test]
fn strict_exits_0_without_flips() {
    let vault = vault_with_verdict(TODAY);
    let cwd = tempfile::tempdir().unwrap();
    let out = regrade(vault.path(), cwd.path(), &["--strict", "--parser", "audit"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn unknown_argument_fails() {
    let vault = vault_with_verdict(TODAY);
    let cwd = tempfile::tempdir().unwrap();
    for args in [&["--bogus"][..], &["--parser", "chat"], &["--idea"]] {
        let out = regrade(vault.path(), cwd.path(), args);
        assert_ne!(out.status.code(), Some(0), "{args:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("usage: idea-vault regrade"),
            "{args:?}"
        );
    }
}

#[test]
fn export_writes_the_fixture_under_the_working_directory() {
    let vault = vault_with_verdict(TODAY);
    let cwd = tempfile::tempdir().unwrap();
    let reference = format!("idea/{RUN}#1");
    let out = regrade(
        vault.path(),
        cwd.path(),
        &["--export", &reference, "two-findings"],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let fixture = cwd
        .path()
        .join("tests/fixtures/raw-outputs/audit/two-findings.md");
    let text = std::fs::read_to_string(&fixture).unwrap();
    assert!(text.starts_with("---\nparser: audit\nn: 2\n"), "{text}");
    assert!(text.contains(AUDIT), "{text}");
}
