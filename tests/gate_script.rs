//! scripts/gate.sh under test (ADR-0041, docs/14-no-mistakes-gate.md): the hook installer, the
//! flagless interface, step 1's intent structure and freshness, step 4's bless refusal and step
//! 7's honesty check. Each test runs the real script inside a scratch git repository with `main`
//! and a feature branch; `cargo` is a stub on PATH that logs its arguments, so there is no test
//! seam inside gate.sh.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; HTC-6 binds shipping code"
)]
mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use support::gate::{allows_rs, clean_tree, copy_scripts, floor, write};
use tempfile::TempDir;

const MARKER: &str = "# idea-vault gate pre-push hook v1";

/// A scratch repository: `<tmp>/repo` holds the tree, `<tmp>/bin/cargo` is the stub, and
/// `<tmp>/cargo.log` records every cargo call.
struct Repo {
    tmp: TempDir,
}

impl Repo {
    fn new() -> Repo {
        let tmp = TempDir::new().unwrap();
        let repo = Repo { tmp };
        let root = repo.root();
        clean_tree(&root);
        copy_scripts(&root);
        let stub = repo.tmp.path().join("bin/cargo");
        write(
            repo.tmp.path(),
            "bin/cargo",
            &format!(
                "#!/usr/bin/env bash\necho \"$*\" >> '{}'\nexit 0\n",
                repo.log_path().display()
            ),
        );
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        repo.git(&["init", "-q", "-b", "main"]);
        fs::create_dir_all(root.join(".git/hooks")).unwrap();
        repo.commit("seed");
        repo
    }

    fn root(&self) -> PathBuf {
        self.tmp.path().join("repo")
    }

    fn log_path(&self) -> PathBuf {
        self.tmp.path().join("cargo.log")
    }

    fn cargo_log(&self) -> String {
        fs::read_to_string(self.log_path()).unwrap_or_default()
    }

    fn command(&self, program: &str) -> Command {
        let mut cmd = Command::new(program);
        let path = format!(
            "{}:{}",
            self.tmp.path().join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        cmd.current_dir(self.root())
            .env("PATH", path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("PARSER_CORPUS_BLESS");
        cmd
    }

    fn git(&self, args: &[&str]) {
        let out = self.command("git").args(args).output().expect("git runs");
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    fn commit(&self, msg: &str) {
        self.git(&["add", "."]);
        self.git(&["commit", "-q", "--no-gpg-sign", "-m", msg]);
    }

    fn write(&self, rel: &str, text: &str) {
        write(&self.root(), rel, text);
    }

    fn gate(&self, args: &[&str]) -> Output {
        self.gate_env(args, &[])
    }

    fn gate_env(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut cmd = self.command("bash");
        cmd.arg("scripts/gate.sh").args(args);
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.output().expect("bash runs")
    }

    /// Switch to a feature branch whose intent is fresh, optionally with extra intent text.
    fn feature(&self, intent_tail: &str) {
        self.git(&["checkout", "-q", "-b", "feature"]);
        self.write(
            "docs/INTENT.md",
            &format!("# Intent — feature work (D1)\n\n## Acceptance criteria\n\n- it works\n{intent_tail}"),
        );
    }
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn hook(dir: &Path) -> PathBuf {
    dir.join("pre-push")
}

#[test]
fn install_hook_writes_executable_marked_hook() {
    let repo = Repo::new();
    let out = repo.gate(&["--install-hook"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let path = hook(&repo.root().join(".git/hooks"));
    let body = fs::read_to_string(&path).unwrap();
    assert!(body.lines().any(|l| l == MARKER), "{body}");
    assert!(body.contains("check-invariants.sh\" --strict"), "{body}");
    let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o755);
    let ran = repo.command(path.to_str().unwrap()).output().unwrap();
    assert_eq!(
        ran.status.code(),
        Some(0),
        "the hook runs the invariants: {}",
        text(&ran)
    );
    assert!(text(&ran).contains("[ok] ollama-url"), "{}", text(&ran));
}

#[test]
fn install_hook_is_idempotent() {
    let repo = Repo::new();
    assert_eq!(repo.gate(&["--install-hook"]).status.code(), Some(0));
    let path = hook(&repo.root().join(".git/hooks"));
    let first = fs::read_to_string(&path).unwrap();
    let again = repo.gate(&["--install-hook"]);
    assert_eq!(again.status.code(), Some(0), "{}", text(&again));
    assert_eq!(fs::read_to_string(&path).unwrap(), first);
}

#[test]
fn install_hook_refuses_foreign_hook() {
    let repo = Repo::new();
    let path = hook(&repo.root().join(".git/hooks"));
    fs::write(&path, "#!/bin/sh\necho mine\n").unwrap();
    let out = repo.gate(&["--install-hook"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert_eq!(fs::read_to_string(&path).unwrap(), "#!/bin/sh\necho mine\n");
}

#[test]
fn install_hook_honours_core_hooks_path() {
    let repo = Repo::new();
    fs::create_dir_all(repo.root().join("myhooks")).unwrap();
    repo.git(&["config", "core.hooksPath", "myhooks"]);
    let out = repo.gate(&["--install-hook"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(hook(&repo.root().join("myhooks")).is_file());
    assert!(!hook(&repo.root().join(".git/hooks")).exists());
}

#[test]
fn install_hook_with_other_flag_is_usage_error() {
    let repo = Repo::new();
    for args in [
        &["--install-hook", "--list"][..],
        &["--list", "--install-hook"],
    ] {
        let out = repo.gate(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("--list") && err.contains("--install-hook"),
            "{err}"
        );
    }
    assert!(!hook(&repo.root().join(".git/hooks")).exists());
}

#[test]
fn gate_rejects_unknown_flag_and_has_no_skip() {
    let repo = Repo::new();
    for args in [
        &["--skip", "3"][..],
        &["--from", "4"],
        &["--strict"],
        &["--no-verify"],
    ] {
        let out = repo.gate(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(repo.cargo_log().is_empty(), "{args:?} ran no step");
    }
    let list = repo.gate(&["--list"]);
    assert_eq!(list.status.code(), Some(0));
    let steps: Vec<String> = String::from_utf8_lossy(&list.stdout)
        .lines()
        .map(|l| l.split('|').nth(1).unwrap().to_string())
        .collect();
    assert_eq!(
        steps,
        [
            "intent",
            "invariants",
            "build",
            "tests",
            "fmt",
            "clippy",
            "honesty"
        ]
    );
}

#[test]
fn intent_without_acceptance_bullet_fails_step1() {
    let repo = Repo::new();
    repo.git(&["checkout", "-q", "-b", "feature"]);
    repo.write(
        "docs/INTENT.md",
        "# Intent — x (ADR-0001)\n\n## Acceptance criteria\n\nnone yet\n",
    );
    let out = repo.gate(&[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out).contains("GATE FAILED at: intent (the top intent block has no"),
        "{}",
        text(&out)
    );
    assert!(repo.cargo_log().is_empty());
}

#[test]
fn intent_without_adr_or_d_token_fails_step1() {
    let repo = Repo::new();
    repo.git(&["checkout", "-q", "-b", "feature"]);
    repo.write(
        "docs/INTENT.md",
        "# Intent — x\n\n## Acceptance criteria\n\n- it works\n",
    );
    let out = repo.gate(&[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out).contains("names no ADR-NNNN or D<n>"),
        "{}",
        text(&out)
    );
    assert!(repo.cargo_log().is_empty());
}

#[test]
fn intent_unchanged_since_main_fails_on_branch() {
    let repo = Repo::new();
    repo.git(&["checkout", "-q", "-b", "feature"]);
    let out = repo.gate(&[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(
        text(&out).contains("unchanged since main"),
        "{}",
        text(&out)
    );
    // A commit on the branch that leaves the intent alone is still stale.
    repo.write("src/more.rs", "pub fn more() {}\n");
    repo.commit("more");
    let out = repo.gate(&[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(
        text(&out).contains("unchanged since main"),
        "{}",
        text(&out)
    );
    assert!(repo.cargo_log().is_empty());
}

#[test]
fn gate_step4_runs_validate_on_a_golden_vault_copy() {
    let repo = Repo::new();
    let out = repo.gate(&[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(
        text(&out).contains("validate: golden vault copy"),
        "{}",
        text(&out)
    );
    let log = repo.cargo_log();
    let calls: Vec<&str> = log.lines().collect();
    let test = calls.iter().position(|l| l.starts_with("test"));
    let validate = calls.iter().position(|l| l.ends_with("-- validate"));
    assert!(
        matches!((test, validate), (Some(t), Some(v)) if t < v),
        "validate runs after cargo test: {log}"
    );
}

#[test]
fn gate_step4_fails_without_the_golden_vault_fixture() {
    let repo = Repo::new();
    fs::remove_dir_all(repo.root().join("tests/fixtures/golden-vault")).unwrap();
    repo.commit("drop the fixture");
    let out = repo.gate(&[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(
        text(&out).contains("GATE FAILED at: tests"),
        "{}",
        text(&out)
    );
}

#[test]
fn gate_step4_fails_on_a_golden_vault_with_no_idea() {
    let repo = Repo::new();
    fs::remove_file(repo.root().join("tests/fixtures/golden-vault/seed/idea.md")).unwrap();
    repo.write(
        "tests/fixtures/golden-vault/seed/notes.txt",
        "not an idea\n",
    );
    repo.commit("empty the fixture");
    let out = repo.gate(&[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(text(&out).contains("holds no idea"), "{}", text(&out));
    assert!(
        !repo.cargo_log().lines().any(|l| l.ends_with("-- validate")),
        "validate never ran on the empty copy"
    );
}

#[test]
fn intent_freshness_skipped_on_main() {
    let repo = Repo::new();
    let out = repo.gate(&[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(
        text(&out).contains("freshness skipped on main"),
        "{}",
        text(&out)
    );
    assert!(repo.cargo_log().contains("clippy"));
}

#[test]
fn unlisted_fixture_change_on_main_fails_step7() {
    // Only freshness is skipped on main; honesty diffs the working tree against HEAD.
    let repo = Repo::new();
    repo.write("tests/fixtures/case.md", "a changed expectation on main\n");
    let out = repo.gate(&[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    let t = text(&out);
    assert!(t.contains("freshness skipped on main"), "{t}");
    assert!(
        t.contains("undeclared expectation change: tests/fixtures/case.md"),
        "{t}"
    );
    assert!(t.contains("GATE FAILED at: honesty"), "{t}");
}

#[test]
fn raised_floor_on_main_needs_a_declaration() {
    let repo = Repo::new();
    let n = floor("CLIPPY_ALLOW_FLOOR");
    let script = repo.root().join("scripts/check-invariants.sh");
    let raised = fs::read_to_string(&script).unwrap().replace(
        &format!("CLIPPY_ALLOW_FLOOR={n}\n"),
        &format!("CLIPPY_ALLOW_FLOOR={}\n", n + 1),
    );
    fs::write(&script, raised).unwrap();
    repo.write("src/allows.rs", &allows_rs(n + 1));
    let out = repo.gate(&[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(
        text(&out).contains("undeclared expectation change: CLIPPY_ALLOW_FLOOR"),
        "{}",
        text(&out)
    );
    // Declared in the top intent block, the same change passes.
    repo.write(
        "docs/INTENT.md",
        &format!(
            "{}\n## Expectation changes\n\n- CLIPPY_ALLOW_FLOOR: one justified allow\n",
            support::gate::INTENT
        ),
    );
    let out = repo.gate(&[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
}

#[test]
fn bless_env_fails_step4_before_cargo_test() {
    let repo = Repo::new();
    repo.feature("");
    let out = repo.gate_env(&[], &[("PARSER_CORPUS_BLESS", "1")]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(
        text(&out).contains("tests (PARSER_CORPUS_BLESS set"),
        "{}",
        text(&out)
    );
    let log = repo.cargo_log();
    assert!(log.lines().any(|l| l.starts_with("build")), "{log}");
    assert!(!log.lines().any(|l| l.starts_with("test")), "{log}");
}

#[test]
fn unlisted_fixture_change_fails_step7() {
    let repo = Repo::new();
    repo.feature("");
    repo.write("tests/fixtures/case.md", "a new expectation\n");
    let out = repo.gate(&[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    let t = text(&out);
    assert!(
        t.contains("undeclared expectation change: tests/fixtures/case.md"),
        "{t}"
    );
    assert!(t.contains("GATE FAILED at: honesty"), "{t}");
    assert!(repo.cargo_log().contains("clippy"), "steps 3-6 ran first");
}

#[test]
fn listed_fixture_change_passes_step7() {
    let repo = Repo::new();
    repo.feature("\n## Expectation changes\n\n- tests/fixtures/case.md: a seeded known-bad case\n");
    repo.write("tests/fixtures/case.md", "a new expectation\n");
    let out = repo.gate(&[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(
        text(&out).contains("1 expectation change(s), all declared"),
        "{}",
        text(&out)
    );
}

#[test]
fn raised_floor_unlisted_fails_step7() {
    let repo = Repo::new();
    repo.feature("");
    let n = floor("CLIPPY_ALLOW_FLOOR");
    let script = repo.root().join("scripts/check-invariants.sh");
    let raised = fs::read_to_string(&script).unwrap().replace(
        &format!("CLIPPY_ALLOW_FLOOR={n}\n"),
        &format!("CLIPPY_ALLOW_FLOOR={}\n", n + 1),
    );
    fs::write(&script, raised).unwrap();
    repo.write("src/allows.rs", &allows_rs(n + 1));
    let out = repo.gate(&[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    let t = text(&out);
    assert!(
        t.contains("undeclared expectation change: CLIPPY_ALLOW_FLOOR"),
        "{t}"
    );
    assert!(
        repo.cargo_log().contains("clippy"),
        "step 2 stayed green: {t}"
    );
}
